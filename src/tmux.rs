use std::{
    cell::RefCell,
    collections::HashMap,
    env, fs,
    hash::{DefaultHasher, Hash, Hasher},
    io::Write,
    os::unix::{
        fs::{MetadataExt as _, PermissionsExt as _},
        process::CommandExt as _,
    },
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::{
    config::{
        AgentProfile, AgentResourcesConfig, ClaudeRelaunchPermissions, ProfileMode, StatusConfig,
    },
    old_sessions::{self, ResumeCandidate},
    status::{self, AgentKind, AgentStatus},
    systemd_scope::{self, PreparedScope},
};

static BUFFER_ID: AtomicU64 = AtomicU64::new(1);
static PANE_IDENTITY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static TMUX_SOCKET_OVERRIDE: RefCell<Option<String>> = const { RefCell::new(None) };
}

struct SocketOverrideRestore(Option<String>);

impl Drop for SocketOverrideRestore {
    fn drop(&mut self) {
        TMUX_SOCKET_OVERRIDE.with(|current| {
            *current.borrow_mut() = self.0.take();
        });
    }
}

const MODEL_MENU_TIMEOUT: Duration = Duration::from_secs(4);
const MODEL_MENU_POLL: Duration = Duration::from_millis(25);
const CLAUDE_SKIP_PERMISSIONS_FLAG: &str = "--dangerously-skip-permissions";
const CLAUDE_PERMISSION_MODE_FLAG: &str = "--permission-mode";
const CLAUDE_BYPASS_PERMISSIONS_MODE: &str = "bypassPermissions";
/// Give interactive TUIs one input turn to finish decoding bracketed paste
/// before Enter arrives. Without this boundary Claude and Codex can consume
/// both terminal writes in one read and leave the pasted text unsubmitted.
const TEXT_PASTE_SUBMIT_SETTLE: Duration = Duration::from_millis(75);

/// One model the installed interactive harness exposes through its native
/// model picker. These ids are fixed atmux control values, never commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnownModel {
    pub id: &'static str,
    pub label: &'static str,
}

/// Model state observed from the owning tmux pane.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelObservation {
    pub version: Option<String>,
    pub current: Option<String>,
    pub effort: Option<String>,
    /// The opaque profile-scoped mode chosen by atmux, when available.
    pub mode: Option<String>,
}

/// Marker error for a harness whose interactive picker no longer matches the
/// narrow versioned protocol atmux knows how to drive.
#[derive(Debug)]
pub struct UnsupportedModelControl(String);

impl std::fmt::Display for UnsupportedModelControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for UnsupportedModelControl {}

/// Marker error for a resume launcher which disappeared or stopped satisfying
/// the owner-local trust boundary before tmux could be invoked.
#[derive(Debug)]
pub(crate) struct ClaudeResumeUnavailable(pub(crate) String);

impl std::fmt::Display for ClaudeResumeUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ClaudeResumeUnavailable {}

const CLAUDE_MODELS: &[KnownModel] = &[
    KnownModel {
        id: "default",
        label: "Default",
    },
    KnownModel {
        id: "opus",
        label: "Opus",
    },
    KnownModel {
        id: "fable",
        label: "Fable",
    },
    KnownModel {
        id: "sonnet",
        label: "Sonnet",
    },
    KnownModel {
        id: "haiku",
        label: "Haiku",
    },
];

const CODEX_MODELS: &[KnownModel] = &[
    KnownModel {
        id: "gpt-5.6-sol",
        label: "GPT-5.6 Sol",
    },
    KnownModel {
        id: "gpt-5.6-terra",
        label: "GPT-5.6 Terra",
    },
    KnownModel {
        id: "gpt-5.6-luna",
        label: "GPT-5.6 Luna",
    },
    KnownModel {
        id: "gpt-5.5",
        label: "GPT-5.5",
    },
    KnownModel {
        id: "gpt-5.4",
        label: "GPT-5.4",
    },
    KnownModel {
        id: "gpt-5.4-mini",
        label: "GPT-5.4 Mini",
    },
    KnownModel {
        id: "gpt-5.3-codex-spark",
        label: "GPT-5.3 Codex Spark",
    },
];

/// The dashboard's own service process is kept in a tmux session on hosts so
/// it survives terminal disconnects. It is infrastructure rather than an
/// agent workspace, so never expose it as a controllable dashboard session.
pub(crate) const RESERVED_SERVICE_SESSION: &str = "atmux-web";

#[derive(Clone, Debug)]
pub struct Session {
    pub name: String,
    pub attached: bool,
    pub windows: u32,
    pub activity: u64,
    pub window_index: u32,
    pub pane_index: u32,
    pub pane_id: String,
    pub pane_pid: u32,
    /// Set-once owner-tmux pane generation used with pane id/pid to reject a
    /// stale/reused pane identity before filesystem mutations.
    pub(crate) pane_identity: String,
    /// Exact descendant CLI process, distinct from an intermediate tmux shell.
    pub(crate) agent_pid: Option<u32>,
    /// Approximate CLI process start in Unix milliseconds, used only to map
    /// this pane to its own agent-native session log.
    pub(crate) agent_started_ms: Option<u64>,
    pub path: PathBuf,
    pub command: String,
    /// A safe, abbreviated descriptor of the command tmux used to start this pane.
    pub launch_command: String,
    pub title: String,
    pub content: String,
    pub content_hash: u64,
    pub agent: AgentKind,
    /// Explicit or safely inferred Claude/Codex profile label.
    pub profile: String,
    /// Stable opaque saved-conversation lease persisted only in tmux metadata.
    pub(crate) resume_lease: Option<String>,
    /// Unique transient systemd scope containing this agent process generation.
    pub systemd_scope: Option<String>,
    /// Scope-level cgroup `MemoryMax`, in bytes.
    pub memory_max_bytes: Option<u64>,
    pub status: AgentStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LivePaneIdentity {
    pub pane_id: String,
    pub pane_pid: u32,
    pub pane_identity: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
struct RawPane {
    name: String,
    attached: bool,
    windows: u32,
    activity: u64,
    window_index: u32,
    window_active: bool,
    pane_index: u32,
    pane_active: bool,
    pane_pid: u32,
    pane_identity: String,
    command: String,
    start_command: String,
    path: PathBuf,
    title: String,
    pane_id: String,
    status_override: String,
    model_override: String,
    agent_version: String,
    profile: String,
    resume_lease: String,
    systemd_scope: String,
    memory_max_bytes: String,
}

impl RawPane {
    fn score(&self) -> u8 {
        u8::from(self.window_active) * 2 + u8::from(self.pane_active)
    }
}

fn pane_rank(pane: &RawPane, agent: AgentKind) -> (bool, u8) {
    (agent != AgentKind::Other, pane.score())
}

#[derive(Clone, Debug, Default)]
pub struct Tmux;

/// One browser-exposed, fixed tmux key action.
///
/// Keeping this as an enum prevents request data from ever becoming a tmux
/// argument. Each variant maps to a single audited tmux key name below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneSpecialKey {
    Up,
    Down,
    Left,
    Right,
    Enter,
    TmuxPrefixTwice,
}

impl PaneSpecialKey {
    #[must_use]
    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Enter => "enter",
            Self::TmuxPrefixTwice => "tmux_prefix_twice",
        }
    }
}

impl Tmux {
    /// Replaces this process with one exact command in a configured agent scope.
    ///
    /// This is the fail-closed bridge used by fixed boot/Quick Resume scripts:
    /// it loads the active configuration itself, requires an enabled memory
    /// policy, preflights the user scope, publishes metadata on the owning pane,
    /// and only then performs an argv-preserving `exec`.
    ///
    /// # Errors
    ///
    /// Returns an error for absent isolation, an unsafe/missing pane identity,
    /// failed scope preflight/metadata, or an empty/unexecutable command.
    pub fn scoped_exec(
        config_path: &Path,
        requested_memory_max_bytes: Option<u64>,
        recovery_service_memory_max_bytes: Option<u64>,
        command: Vec<String>,
    ) -> Result<()> {
        if command.is_empty() {
            bail!("scoped-exec requires a command after --");
        }
        if requested_memory_max_bytes.is_some() && recovery_service_memory_max_bytes.is_some() {
            bail!("scoped-exec worker and recovery service caps are mutually exclusive");
        }
        let (config, _) = crate::config::Config::load(Some(config_path))?;
        if config.agent_resources.memory_max_bytes.is_none() {
            bail!(
                "scoped-exec requires [agent_resources].memory_max_bytes; recovery is fail-closed"
            );
        }
        let pane_id = env::var("TMUX_PANE")
            .ok()
            .filter(|pane_id| valid_tmux_pane_id(pane_id))
            .context("scoped-exec requires a valid TMUX_PANE")?;
        let scope = if let Some(service_memory_max_bytes) = recovery_service_memory_max_bytes {
            systemd_scope::prepare_recovery_service(
                &config.agent_resources,
                service_memory_max_bytes,
                &pane_id,
            )?
        } else {
            systemd_scope::prepare_override(
                &config.agent_resources,
                requested_memory_max_bytes,
                &pane_id,
            )?
        };
        let invocation = scope.wrap(command)?;
        publish_scope_metadata(&pane_id, &scope)?;
        let (program, arguments) = invocation
            .split_first()
            .context("scoped-exec produced an empty command")?;
        let error = Command::new(program).args(arguments).exec();
        Err(error).with_context(|| format!("could not execute scoped agent through {program}"))
    }

    /// Probes configured agent cgroup support without touching tmux.
    ///
    /// The transient probe scope is bounded, immediately collected, and uses
    /// the same `MemoryMax` property as a real launch.
    ///
    /// # Errors
    ///
    /// Returns an error when isolation is configured but the systemd user
    /// manager, scope support, or `MemoryMax` property is unavailable.
    pub fn check_agent_resources(resources: &AgentResourcesConfig) -> Result<Option<u64>> {
        systemd_scope::prepare(resources, "doctor").map(|scope| scope.memory_max_bytes())
    }

    /// Reports the largest override this owner can advertise after clamping
    /// the configured policy to the current host and inherited cgroup limit.
    /// A real launch repeats the check to close the observation/use race.
    ///
    /// # Errors
    ///
    /// Returns an error when an override is configured but the effective
    /// cgroup-v2/host ceiling cannot be read safely.
    pub fn check_agent_memory_override_ceiling(
        resources: &AgentResourcesConfig,
    ) -> Result<Option<u64>> {
        systemd_scope::advertised_override_ceiling(resources)
    }

    /// Runs an integration probe against one explicit tmux socket without
    /// mutating process-global environment or the user's default server.
    ///
    /// This is public only so black-box integration tests can exercise the real
    /// tmux transport. Application requests never select a socket.
    #[doc(hidden)]
    pub fn with_socket_for_test<T>(socket: &str, action: impl FnOnce() -> Result<T>) -> Result<T> {
        if !valid_socket_name(socket) {
            bail!("invalid tmux test socket name");
        }
        let prior = TMUX_SOCKET_OVERRIDE.with(|current| current.replace(Some(socket.to_owned())));
        let _restore = SocketOverrideRestore(prior);
        action()
    }

    /// Verifies that a working tmux executable is available.
    ///
    /// # Errors
    ///
    /// Returns an error when tmux is missing or exits unsuccessfully.
    pub fn check() -> Result<()> {
        let output = tmux_command()
            .arg("-V")
            .output()
            .context("tmux is required but was not found")?;
        check_output(&output, "tmux -V").map(|_| ())
    }

    /// Reads active-pane metadata and inferred agent state for every session.
    ///
    /// # Errors
    ///
    /// Returns an error when tmux session metadata cannot be queried.
    pub fn sessions(
        &self,
        previous_hashes: &HashMap<String, u64>,
        status_config: &StatusConfig,
    ) -> Result<Vec<Session>> {
        self.sessions_with_capture(previous_hashes, status_config, 36)
    }

    /// Reads session metadata while retaining the requested number of pane lines.
    ///
    /// # Errors
    ///
    /// Returns an error when tmux session metadata cannot be queried.
    #[allow(clippy::too_many_lines)] // One scan keeps pane metadata and process state coherent.
    pub fn sessions_with_capture(
        &self,
        previous_hashes: &HashMap<String, u64>,
        status_config: &StatusConfig,
        capture_lines: usize,
    ) -> Result<Vec<Session>> {
        let format = [
            "#{session_name}",
            "#{session_attached}",
            "#{session_windows}",
            "#{session_activity}",
            "#{window_index}",
            "#{window_active}",
            "#{pane_index}",
            "#{pane_active}",
            "#{pane_pid}",
            "#{pane_current_command}",
            "#{pane_start_command}",
            "#{pane_current_path}",
            "#{pane_title}",
            "#{pane_id}",
            "#{@atmux_status}",
            "#{@atmux_model}",
            "#{@atmux_agent_version}",
            "#{@atmux_profile}",
            "#{@atmux_resume_lease}",
            "#{@atmux_identity}",
            "#{@atmux_systemd_scope}",
            "#{@atmux_memory_max_bytes}",
        ]
        .join("\t");
        let (raw_output, summary) = Self::run(["list-panes", "-a", "-F", &format])?;
        if !raw_output.status.success() && is_missing_server_output(&raw_output) {
            return Ok(Vec::new());
        }
        let output = check_output(&raw_output, &summary)?;
        let process_table = ProcessTable::load();
        let selected = select_session_panes(&output, &process_table);

        let mut sessions = Vec::with_capacity(selected.len());
        for (pane, detected_agent) in selected.into_values() {
            let content = self
                .capture(&pane.pane_id, capture_lines)
                .unwrap_or_default();
            // Claude's current launcher replaces `claude` with a versioned
            // executable name. If process inspection could not identify the
            // harness, the bounded pane banner remains an authoritative
            // fallback and makes existing sessions controllable too.
            let agent = if detected_agent == AgentKind::Other {
                status::detect_kind(&pane.command, &content)
            } else {
                detected_agent
            };
            let (agent_pid, agent_started_ms) = process_table
                .agent_process_under(pane.pane_pid, agent)
                .map_or((None, None), |(pid, started)| (Some(pid), started));
            remember_model_metadata(
                &pane.pane_id,
                agent,
                &content,
                &pane.model_override,
                &pane.agent_version,
            );
            let content_hash = hash(&content);
            let changed = previous_hashes
                .get(&pane.pane_id)
                .is_some_and(|previous| *previous != content_hash);
            let agent_status = status::classify(
                agent,
                &content,
                &pane.title,
                &pane.status_override,
                changed,
                status_config,
            );
            let pane_identity =
                ensure_pane_identity(&pane.pane_id, &pane.pane_identity).unwrap_or_default();
            let (systemd_scope, memory_max_bytes) =
                scope_metadata(&pane.systemd_scope, &pane.memory_max_bytes);
            sessions.push(Session {
                name: pane.name,
                attached: pane.attached,
                windows: pane.windows,
                activity: pane.activity,
                window_index: pane.window_index,
                pane_index: pane.pane_index,
                pane_id: pane.pane_id,
                pane_pid: pane.pane_pid,
                pane_identity,
                agent_pid,
                agent_started_ms,
                path: pane.path,
                command: pane.command,
                launch_command: launch_command_label(&pane.start_command),
                title: pane.title,
                content,
                content_hash,
                agent,
                profile: agent_profile_label(&pane.start_command, &pane.profile, agent),
                resume_lease: valid_resume_lease(&pane.resume_lease)
                    .then(|| pane.resume_lease.clone()),
                systemd_scope,
                memory_max_bytes,
                status: agent_status,
            });
        }
        sessions.sort_by(|left, right| {
            right
                .status
                .eq(&AgentStatus::Waiting)
                .cmp(&left.status.eq(&AgentStatus::Waiting))
                .then_with(|| right.activity.cmp(&left.activity))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(sessions)
    }

    /// Captures recent plain text from a tmux pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the target pane does not exist or tmux cannot capture it.
    pub fn capture(&self, pane_id: &str, lines: usize) -> Result<String> {
        Self::output([
            "capture-pane",
            "-p",
            "-t",
            pane_id,
            "-S",
            &format!("-{}", lines.max(1)),
        ])
    }

    /// Reads the exact current identity and cwd for one owner-local pane. The
    /// id, tmux creation epoch, and shell pid together prevent a stale cached
    /// pane from authorizing a filesystem mutation after kill/reuse.
    ///
    /// # Errors
    ///
    /// Returns an error for a malformed pane id or an unexpected tmux failure.
    pub(crate) fn live_pane_identity(pane_id: &str) -> Result<Option<LivePaneIdentity>> {
        if !valid_tmux_pane_id(pane_id) {
            bail!("invalid tmux pane id");
        }
        let format = "#{pane_id}\t#{pane_pid}\t#{@atmux_identity}\t#{pane_current_path}";
        let (output, summary) = Self::run(["display-message", "-p", "-t", pane_id, format])?;
        if !output.status.success() {
            let stderr = output_stderr(&output);
            if is_missing_server_message(&stderr)
                || stderr.contains("can't find pane")
                || stderr.contains("can't find session")
                || stderr.contains("no such pane")
            {
                return Ok(None);
            }
            return check_output(&output, &summary).map(|_| None);
        }
        let value = check_output(&output, &summary)?;
        let mut fields = value.splitn(4, '\t');
        let (Some(observed_id), Some(pid), Some(identity), Some(path)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            bail!("tmux returned malformed live pane metadata");
        };
        if observed_id != pane_id
            || !valid_pane_identity(identity)
            || path.is_empty()
            || path.chars().any(char::is_control)
        {
            bail!("tmux returned unsafe live pane metadata");
        }
        Ok(Some(LivePaneIdentity {
            pane_id: observed_id.to_owned(),
            pane_pid: pid.parse().context("tmux returned an invalid pane pid")?,
            pane_identity: identity.to_owned(),
            path: PathBuf::from(path),
        }))
    }

    /// Creates a detached session running the chosen agent profile.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid directory or a failed tmux launch.
    pub(crate) fn launch(
        name: &str,
        directory: &Path,
        profile: &AgentProfile,
        mode: Option<&ProfileMode>,
        // Consume the one-launch plan so a successful preflight cannot be
        // reused for another process generation.
        #[allow(clippy::needless_pass_by_value)] scope: PreparedScope,
    ) -> Result<()> {
        Self::launch_inner(name, directory, profile, mode, None, None, scope)
    }

    /// Creates a detached session that resumes one owner-revalidated native
    /// Claude or Codex conversation.
    ///
    /// # Errors
    ///
    /// Returns an error for a harness mismatch, an ambiguous configured resume
    /// selector, invalid server-derived resume data, or failed tmux launch.
    pub(crate) fn launch_resumed(
        name: &str,
        directory: &Path,
        profile: &AgentProfile,
        mode: Option<&ProfileMode>,
        resume: &ResumeCandidate,
        resume_lease: &str,
        scope: PreparedScope,
    ) -> Result<()> {
        if !valid_resume_lease(resume_lease) {
            bail!("saved-conversation lease is invalid");
        }
        Self::launch_inner(
            name,
            directory,
            profile,
            mode,
            Some(resume),
            Some(resume_lease),
            scope,
        )
    }

    #[allow(clippy::needless_pass_by_value)] // Enforce one preflight per process generation.
    fn launch_inner(
        name: &str,
        directory: &Path,
        profile: &AgentProfile,
        mode: Option<&ProfileMode>,
        resume: Option<&ResumeCandidate>,
        resume_lease: Option<&str>,
        scope: PreparedScope,
    ) -> Result<()> {
        if !command_available(&profile.command) {
            bail!(
                "agent command was not found or is not executable: {}",
                profile.command
            );
        }
        let invocation = Self::build_launch_invocation(profile, mode, resume)?;
        let invocation = scope.wrap(invocation)?;
        let shell_command = shell_words::join(invocation);
        let directory = directory
            .to_str()
            .with_context(|| format!("directory is not valid UTF-8: {}", directory.display()))?;
        // Publish every durable claim on a harmless placeholder pane before
        // the native agent starts. If atmux exits mid-launch, restart recovery
        // can either see the lease or see no resumed conversation at all.
        let created = Self::output([
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{session_id}\t#{pane_id}",
            "-s",
            name,
            "-c",
            directory,
            "/bin/sleep 2147483647",
        ])?;
        let Some((session_id, pane_id)) = parse_tmux_created_target(&created) else {
            bail!("tmux did not return a valid identity for session {name}");
        };
        if let Err(error) = Self::output([
            "set-option",
            "-p",
            "-t",
            &pane_id,
            "@atmux_profile",
            &profile.name,
        ]) {
            let _ = Self::output(["kill-session", "-t", &session_id]);
            return Err(error);
        }
        if let Some(mode) = mode {
            for (key, value) in [
                ("@atmux_mode", Some(mode.id.as_str())),
                ("@atmux_model", Some(mode.model.as_str())),
                ("@atmux_effort", mode.effort.as_deref()),
                ("@atmux_service_tier", mode.service_tier.as_deref()),
            ] {
                let Some(value) = value else {
                    continue;
                };
                if let Err(error) = Self::output(["set-option", "-p", "-t", &pane_id, key, value]) {
                    let _ = Self::output(["kill-session", "-t", &session_id]);
                    return Err(error);
                }
            }
        }
        if let Some(lease) = resume_lease
            && let Err(error) = Self::output([
                "set-option",
                "-p",
                "-t",
                &pane_id,
                "@atmux_resume_lease",
                lease,
            ])
        {
            let _ = Self::output(["kill-session", "-t", &session_id]);
            return Err(error);
        }
        if let Err(error) = publish_scope_metadata(&pane_id, &scope) {
            let _ = Self::output(["kill-session", "-t", &session_id]);
            return Err(error);
        }
        if let Err(error) = Self::output([
            "respawn-pane",
            "-k",
            "-t",
            &pane_id,
            "-c",
            directory,
            &shell_command,
        ]) {
            let _ = Self::output(["kill-session", "-t", &session_id]);
            return Err(error);
        }
        if let Err(error) = Self::wait_until_session_is_stable(&session_id, name) {
            let _ = Self::output(["kill-session", "-t", &session_id]);
            return Err(error);
        }
        Ok(())
    }

    /// Checks the owner tmux server for one persistent saved-conversation lease.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid lease or an unexpected tmux failure.
    pub(crate) fn resume_lease_active(lease: &str) -> Result<bool> {
        if !valid_resume_lease(lease) {
            bail!("saved-conversation lease is invalid");
        }
        let (output, summary) = Self::run(["list-panes", "-a", "-F", "#{@atmux_resume_lease}"])?;
        if !output.status.success() && is_missing_server_output(&output) {
            return Ok(false);
        }
        Ok(check_output(&output, &summary)?
            .lines()
            .any(|active| active == lease))
    }

    fn build_launch_invocation(
        profile: &AgentProfile,
        mode: Option<&ProfileMode>,
        resume: Option<&ResumeCandidate>,
    ) -> Result<Vec<String>> {
        Self::build_invocation(profile, mode, resume)
    }

    fn build_invocation(
        profile: &AgentProfile,
        mode: Option<&ProfileMode>,
        resume: Option<&ResumeCandidate>,
    ) -> Result<Vec<String>> {
        let mut arguments = profile.args.clone();
        let claude_permissions = profile
            .harness
            .eq_ignore_ascii_case("claude")
            .then(|| profile.effective_claude_relaunch_permissions());
        let atmux_manages_claude_permissions =
            claude_permissions == Some(ClaudeRelaunchPermissions::AtmuxInjects);
        if let Some(mode) = mode {
            if !valid_model_id(&mode.model) {
                bail!("profile mode has an invalid model id");
            }
            let mode_arguments = match profile.harness.to_ascii_lowercase().as_str() {
                "claude" => {
                    if mode.service_tier.is_some() {
                        bail!("Claude profile modes cannot set a service tier");
                    }
                    let mut mode_arguments = vec!["--model".to_owned(), mode.model.clone()];
                    if let Some(effort) = &mode.effort {
                        if !valid_claude_effort(effort) {
                            bail!("Claude profile mode has unsupported effort");
                        }
                        mode_arguments.extend(["--effort".to_owned(), effort.clone()]);
                    }
                    mode_arguments
                }
                "codex" => {
                    let mut mode_arguments = vec!["--model".to_owned(), mode.model.clone()];
                    if let Some(effort) = &mode.effort {
                        if !valid_codex_effort(effort) {
                            bail!("profile mode has unsupported Codex effort");
                        }
                        mode_arguments.extend([
                            "-c".to_owned(),
                            format!("model_reasoning_effort=\"{effort}\""),
                        ]);
                    }
                    if let Some(tier) = &mode.service_tier {
                        if tier != "fast" {
                            bail!("profile mode has unsupported Codex service tier");
                        }
                        mode_arguments
                            .extend(["-c".to_owned(), format!("service_tier=\"{tier}\"")]);
                    }
                    mode_arguments
                }
                _ => bail!("profile harness does not support configured modes"),
            };
            if atmux_manages_claude_permissions {
                insert_before_option_terminator(&mut arguments, mode_arguments);
            } else {
                // Opaque launchers and non-Claude harnesses retain their
                // established ordering. Atmux-managed Claude arguments move
                // ahead of the native option terminator so they remain active.
                arguments.extend(mode_arguments);
            }
        }
        if let Some(resume) = resume {
            let harness = profile.harness.to_ascii_lowercase();
            if harness != resume.harness().as_str() {
                bail!("saved conversation does not match the selected profile harness");
            }
            let has_selector = match harness.as_str() {
                "claude" if claude_permissions == Some(ClaudeRelaunchPermissions::AtmuxInjects) => {
                    active_arguments(&arguments)
                        .iter()
                        .any(|arg| is_claude_resume_selector(arg))
                }
                "claude" => arguments.iter().any(|arg| is_claude_resume_selector(arg)),
                "codex" => arguments.iter().any(|arg| arg == "resume"),
                _ => true,
            };
            if has_selector {
                bail!("selected profile already defines a resume selector");
            }
            let resume_arguments = old_sessions::resume_arguments(resume)?;
            if atmux_manages_claude_permissions {
                insert_before_option_terminator(&mut arguments, resume_arguments);
            } else {
                arguments.extend(resume_arguments);
            }
        }
        if atmux_manages_claude_permissions {
            normalize_claude_permission_arguments(&mut arguments);
        }
        let mut invocation = vec!["env".to_owned()];
        invocation.extend(
            profile
                .env
                .iter()
                .map(|(key, value)| format!("{key}={value}")),
        );
        invocation.push(profile.command.clone());
        invocation.extend(arguments);
        Ok(invocation)
    }

    /// Replaces one stopped-or-waiting Claude process in place with the
    /// current `claude` launcher and its validated native conversation.
    ///
    /// The caller supplies only server-derived values.  In particular, this
    /// never replays a pane's raw start command: that command can pin an old
    /// versioned executable or contain custom environment and arguments.
    ///
    /// # Errors
    ///
    /// Returns an error when the current Claude launcher is unavailable, the
    /// server-derived values are malformed, or tmux rejects the respawn.
    #[allow(clippy::needless_pass_by_value)] // Enforce one preflight per process generation.
    pub(crate) fn resume_claude(
        pane_id: &str,
        directory: &Path,
        claude_program: &Path,
        config_dir: &Path,
        session_id: &str,
        // Consume the one-launch plan so a successful preflight cannot be
        // reused for another process generation.
        scope: PreparedScope,
    ) -> Result<()> {
        let claude_program = crate::config::revalidate_resume_claude_program(claude_program)
            .ok_or_else(|| {
                ClaudeResumeUnavailable(
                    "the current Claude launcher is unavailable on this machine".to_owned(),
                )
            })?;
        let directory = directory
            .to_str()
            .with_context(|| format!("directory is not valid UTF-8: {}", directory.display()))?;
        let invocation = claude_resume_invocation(&claude_program, config_dir, session_id)?;
        let invocation = scope.wrap(invocation)?;
        let command = shell_words::join(invocation);
        publish_scope_metadata(pane_id, &scope)?;
        Self::output(respawn_pane_args(pane_id, directory, &command))?;
        Ok(())
    }

    /// Replaces one exact idle native Claude/Codex process after its owner CLI
    /// changed. All launch values are server-resolved configuration and native
    /// log identity; the old pane start command is never replayed.
    #[allow(clippy::needless_pass_by_value)] // Enforce one preflight per process generation.
    #[allow(clippy::too_many_arguments)] // All values are independently revalidated owner state.
    pub(crate) fn resume_after_cli_update(
        pane_id: &str,
        directory: &Path,
        launcher: &Path,
        harness: crate::auto_update::Harness,
        profile: &AgentProfile,
        mode: &ProfileMode,
        target: &crate::transcript::NativeResumeTarget,
        // Consume the one-launch plan so a successful preflight cannot be
        // reused for another process generation.
        scope: PreparedScope,
    ) -> Result<()> {
        if !crate::auto_update::revalidate_launcher(harness, launcher) {
            bail!("the updated native launcher changed during maintenance preflight");
        }
        let expected_harness = harness.name();
        if !profile.harness.eq_ignore_ascii_case(expected_harness)
            || !(profile.command == expected_harness
                || Path::new(&profile.command).canonicalize().ok().as_deref() == Some(launcher))
        {
            bail!("pane profile is not provably bound to the updated native CLI");
        }
        let config_key = match harness {
            crate::auto_update::Harness::Claude => "CLAUDE_CONFIG_DIR",
            crate::auto_update::Harness::Codex => "CODEX_HOME",
        };
        let resume_args = crate::auto_update::resume_arguments(harness, &target.session_id)?;
        let configured = profile
            .env
            .get(config_key)
            .map(PathBuf::from)
            .and_then(|path| path.canonicalize().ok());
        if configured.as_deref() != target.config_dir.canonicalize().ok().as_deref() {
            bail!("pane profile no longer selects its exact native session store");
        }
        let has_selector = match harness {
            crate::auto_update::Harness::Claude => active_arguments(&profile.args)
                .iter()
                .any(|arg| is_claude_resume_selector(arg)),
            crate::auto_update::Harness::Codex => profile.args.iter().any(|arg| arg == "resume"),
        };
        if has_selector {
            bail!("pane profile already defines a native resume selector");
        }
        let mut exact_profile = profile.clone();
        exact_profile.command = launcher.to_string_lossy().into_owned();
        let invocation =
            build_native_relaunch_invocation(&exact_profile, mode, harness, resume_args)?;
        let invocation = scope.wrap(invocation)?;
        let command = shell_words::join(invocation);
        let directory = directory
            .to_str()
            .with_context(|| format!("directory is not valid UTF-8: {}", directory.display()))?;
        publish_scope_metadata(pane_id, &scope)?;
        Self::output(respawn_pane_args(pane_id, directory, &command))?;
        Ok(())
    }

    /// Switches the current tmux client to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when no client or target session can be found.
    pub fn switch(&self, name: &str) -> Result<()> {
        Self::output(["switch-client", "-t", name]).map(|_| ())
    }

    /// Opens an interactive attachment to a session in a tmux popup.
    ///
    /// # Errors
    ///
    /// Returns an error outside tmux, for a malformed tmux environment, or when the popup fails.
    pub fn popup(&self, name: &str) -> Result<()> {
        let tmux_environment = env::var("TMUX").context("quick edit requires atmux inside tmux")?;
        let command = popup_attach_command(name, &tmux_environment)?;
        let title = format!(" atmux · {name} ");
        Self::output([
            "display-popup",
            "-E",
            "-w",
            "94%",
            "-h",
            "92%",
            "-T",
            &title,
            &command,
        ])
        .map(|_| ())
    }

    /// Attaches the process terminal to a tmux session until it detaches.
    ///
    /// # Errors
    ///
    /// Returns an error when tmux cannot attach or exits unsuccessfully.
    pub fn attach(name: &str) -> Result<()> {
        let status = tmux_command()
            .args(["attach-session", "-t", name])
            .status()
            .with_context(|| format!("failed to attach to tmux session {name}"))?;
        if !status.success() {
            bail!("tmux attach-session exited with {status}");
        }
        Ok(())
    }

    /// Kills one named tmux session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or tmux rejects the request.
    pub fn kill(&self, name: &str) -> Result<()> {
        Self::output(["kill-session", "-t", &format!("={name}")]).map(|_| ())
    }

    /// Pastes literal text into a pane and optionally submits it.
    ///
    /// # Errors
    ///
    /// Returns an error for oversized/NUL-containing input or a failed tmux operation.
    pub fn send_text(&self, pane_id: &str, text: &str, submit: bool) -> Result<()> {
        Self::send_text_checked(pane_id, text, submit, || Ok(()))
    }

    /// Sends text with a caller-supplied pane-generation check immediately
    /// before paste and again before submit.
    pub(crate) fn send_text_checked(
        pane_id: &str,
        text: &str,
        submit: bool,
        mut validate_target: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        const MAX_INPUT_BYTES: usize = 64 * 1024;
        if text.len() > MAX_INPUT_BYTES {
            bail!("message exceeds the {MAX_INPUT_BYTES}-byte limit");
        }
        if text.contains('\0') {
            bail!("message cannot contain a NUL byte");
        }

        let buffer = format!(
            "atmux-{}-{}",
            std::process::id(),
            BUFFER_ID.fetch_add(1, Ordering::Relaxed)
        );
        let mut child = tmux_command()
            .args(["load-buffer", "-b", &buffer, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to run tmux load-buffer")?;
        let write_result = child
            .stdin
            .take()
            .context("tmux load-buffer stdin was unavailable")?
            .write_all(text.as_bytes())
            .context("failed to write message to tmux buffer");
        let output = child
            .wait_with_output()
            .context("failed to wait for tmux load-buffer")?;
        write_result?;
        check_output(&output, "tmux load-buffer")?;

        if let Err(error) = validate_target() {
            let _ = Self::output(["delete-buffer", "-b", &buffer]);
            return Err(error);
        }
        if let Err(error) = Self::output(paste_buffer_args(&buffer, pane_id)) {
            let _ = Self::output(["delete-buffer", "-b", &buffer]);
            return Err(error);
        }
        if submit {
            thread::sleep(TEXT_PASTE_SUBMIT_SETTLE);
            validate_target()?;
        }
        for key in submission_keys(submit) {
            Self::output(["send-keys", "-t", pane_id, key])?;
        }
        Ok(())
    }

    /// Reads the owner-local auto-compact claim persisted on a pane.
    ///
    /// Quiet lookup distinguishes a genuinely absent option from a failed
    /// tmux command; callers must not compact when this read fails.
    pub(crate) fn auto_compact_marker(pane_id: &str) -> Result<Option<String>> {
        let value = Self::output([
            "show-options",
            "-p",
            "-q",
            "-v",
            "-t",
            pane_id,
            crate::auto_compact::MARKER_OPTION,
        ])?;
        Ok((!value.is_empty()).then_some(value))
    }

    /// Persists an auto-compact claim before the literal command is sent.
    pub(crate) fn set_auto_compact_marker(pane_id: &str, marker: &str) -> Result<()> {
        if marker.len() > 160 || !marker.is_ascii() || marker.contains(['\n', '\r', '\0']) {
            bail!("auto-compact marker is malformed");
        }
        Self::output([
            "set-option",
            "-p",
            "-t",
            pane_id,
            crate::auto_compact::MARKER_OPTION,
            marker,
        ])
        .map(|_| ())
    }

    /// Clears the durable claim after the native log proves context reset.
    pub(crate) fn clear_auto_compact_marker(pane_id: &str) -> Result<()> {
        Self::output([
            "set-option",
            "-p",
            "-u",
            "-t",
            pane_id,
            crate::auto_compact::MARKER_OPTION,
        ])
        .map(|_| ())
    }

    /// Submits the fixed `/compact` text and Enter as one tmux client command
    /// list. Once its durable marker exists, any failure is delivery-ambiguous
    /// and the caller must retain the marker rather than retry.
    pub(crate) fn deliver_auto_compact(pane_id: &str) -> Result<()> {
        Self::output(auto_compact_delivery_args(pane_id)).map(|_| ())
    }

    pub(crate) fn cli_update_marker(pane_id: &str) -> Result<Option<String>> {
        let value = Self::output([
            "show-options",
            "-p",
            "-q",
            "-v",
            "-t",
            pane_id,
            crate::auto_update::PENDING_OPTION,
        ])?;
        Ok((!value.is_empty()).then_some(value))
    }

    pub(crate) fn set_cli_update_marker(pane_id: &str, marker: &str) -> Result<()> {
        if crate::auto_update::PendingMarker::parse(marker).is_none() {
            bail!("CLI update marker is malformed");
        }
        Self::output([
            "set-option",
            "-p",
            "-t",
            pane_id,
            crate::auto_update::PENDING_OPTION,
            marker,
        ])
        .map(|_| ())
    }

    pub(crate) fn clear_cli_update_marker(pane_id: &str) -> Result<()> {
        Self::output([
            "set-option",
            "-p",
            "-u",
            "-t",
            pane_id,
            crate::auto_update::PENDING_OPTION,
        ])
        .map(|_| ())
    }

    pub(crate) fn cli_update_service_tier(pane_id: &str) -> Result<Option<String>> {
        let value = Self::output([
            "show-options",
            "-p",
            "-q",
            "-v",
            "-t",
            pane_id,
            "@atmux_service_tier",
        ])?;
        Ok((!value.is_empty()).then_some(value))
    }

    /// Reads the durable sequence advanced before every atmux pane mutation.
    pub(crate) fn pane_mutation_sequence(pane_id: &str) -> Result<u64> {
        let value = Self::output([
            "show-options",
            "-p",
            "-q",
            "-v",
            "-t",
            pane_id,
            "@atmux_mutation_sequence",
        ])?;
        if value.is_empty() {
            return Ok(0);
        }
        value.parse().context("pane mutation sequence is malformed")
    }

    /// Advances the sequence before delivery. If atmux crashes after this
    /// write, pending maintenance plans are invalidated rather than replayed.
    pub(crate) fn advance_pane_mutation_sequence(pane_id: &str) -> Result<u64> {
        let next = Self::pane_mutation_sequence(pane_id)?
            .checked_add(1)
            .context("pane mutation sequence is exhausted")?;
        Self::output([
            "set-option",
            "-p",
            "-t",
            pane_id,
            "@atmux_mutation_sequence",
            &next.to_string(),
        ])?;
        Ok(next)
    }

    /// Submits the current agent composer without pasting additional text.
    ///
    /// Image-aware TUIs may need a moment after a paste to convert a local path
    /// into their native attachment object, so attachment delivery invokes this
    /// separately after that bounded settle interval.
    ///
    /// # Errors
    ///
    /// Returns an error when tmux cannot send Enter to the pane.
    pub fn submit(&self, pane_id: &str) -> Result<()> {
        Self::output(submit_args(pane_id)).map(|_| ())
    }

    /// Sends the standard agent-interrupt key to a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the pane does not exist or tmux rejects the key.
    pub fn interrupt(&self, pane_id: &str) -> Result<()> {
        Self::output(["send-keys", "-t", pane_id, "Escape"]).map(|_| ())
    }

    /// Sends one fixed interactive key, or the existing fixed tmux-prefix
    /// sequence, to a pane.
    ///
    /// Request strings are converted to [`PaneSpecialKey`] before reaching
    /// this boundary, so no browser-controlled value can become a command or
    /// tmux key argument.
    pub(crate) fn send_special_key(&self, pane_id: &str, key: PaneSpecialKey) -> Result<()> {
        if key == PaneSpecialKey::TmuxPrefixTwice {
            return self.tmux_prefix_twice(pane_id);
        }
        Self::output(special_key_args(pane_id, key)).map(|_| ())
    }

    /// Sends the literal `Ctrl+B` key sequence twice to a pane.
    ///
    /// This is deliberately a fixed sequence rather than a generic browser
    /// controlled key-injection API.
    ///
    /// # Errors
    ///
    /// Returns an error when the pane does not exist or tmux rejects either key.
    pub fn tmux_prefix_twice(&self, pane_id: &str) -> Result<()> {
        for key in tmux_prefix_twice_keys() {
            Self::output(["send-keys", "-t", pane_id, key])?;
        }
        Ok(())
    }

    /// Reports the running harness version and current model from the owning
    /// pane. An atmux-applied model is retained as a tmux pane option so an
    /// atmux web-process restart does not forget it.
    #[must_use]
    pub fn model_observation(
        &self,
        pane_id: &str,
        agent: AgentKind,
        content: &str,
    ) -> ModelObservation {
        let stored = Self::output(["show-options", "-p", "-v", "-t", pane_id, "@atmux_model"])
            .ok()
            .filter(|value| valid_model_id(value));
        let stored_version = Self::output([
            "show-options",
            "-p",
            "-v",
            "-t",
            pane_id,
            "@atmux_agent_version",
        ])
        .ok()
        .filter(|value| valid_version(value));
        let effort = Self::output(["show-options", "-p", "-v", "-t", pane_id, "@atmux_effort"])
            .ok()
            .filter(|value| valid_effort_id(value));
        let mode = Self::output(["show-options", "-p", "-v", "-t", pane_id, "@atmux_mode"])
            .ok()
            .filter(|value| valid_mode_id(value));
        let mut observation =
            observe_model(agent, content, stored.as_deref(), stored_version.as_deref());
        observation.effort = displayed_effort(agent, content).or(effort);
        observation.mode = mode;
        observation
    }

    /// Switches a running Claude or Codex TUI through its validated native
    /// model picker. No browser value is ever interpreted as shell text or a
    /// generic tmux key sequence.
    ///
    /// # Errors
    ///
    /// Returns an error when the pane disappears, its picker no longer matches
    /// the installed-version protocol, or tmux rejects a fixed operation.
    pub fn switch_model(
        &self,
        pane_id: &str,
        agent: AgentKind,
        version: &str,
        mode: &ProfileMode,
    ) -> Result<()> {
        let model = &mode.model;
        if !valid_model_id(model)
            || !known_models(agent, version)
                .iter()
                .any(|candidate| candidate.id == model)
        {
            return Err(UnsupportedModelControl(format!(
                "{model} is not switchable by {agent} {version}"
            ))
            .into());
        }
        match agent {
            AgentKind::Claude
                if mode.service_tier.is_none()
                    && mode.effort.as_deref().is_none_or(valid_claude_effort) =>
            {
                self.switch_claude_model(pane_id, model)?;
                if let Some(effort) = mode.effort.as_deref() {
                    self.switch_claude_effort(pane_id, effort)?;
                }
            }
            AgentKind::Codex
                if mode.service_tier.is_none()
                    && mode.effort.as_deref().is_none_or(|effort| effort != "none") =>
            {
                self.switch_codex_model(pane_id, model, mode.effort.as_deref())?;
            }
            AgentKind::Claude | AgentKind::Codex => {
                return Err(UnsupportedModelControl(
                    "this profile mode is available only when launching a new session".to_owned(),
                )
                .into());
            }
            AgentKind::Other => {
                return Err(UnsupportedModelControl(
                    "model switching is available only for Claude and Codex panes".to_owned(),
                )
                .into());
            }
        }
        Self::output(["set-option", "-p", "-t", pane_id, "@atmux_model", model])?;
        Self::output(["set-option", "-p", "-t", pane_id, "@atmux_mode", &mode.id])?;
        if let Some(effort) = &mode.effort {
            Self::output(["set-option", "-p", "-t", pane_id, "@atmux_effort", effort])?;
        }
        Ok(())
    }

    fn switch_claude_model(&self, pane_id: &str, model: &str) -> Result<()> {
        self.send_text(pane_id, "/model", true)?;
        let menu = wait_for_capture(pane_id, |content| {
            menu_selection(content, "Select model", CLAUDE_MODELS)
        })?;
        let target = model_index(CLAUDE_MODELS, model)?;
        move_menu_selection(pane_id, menu, target)?;
        // Claude's `s` action changes only this running session. Enter would
        // also rewrite the user's default for every future session.
        Self::output(["send-keys", "-t", pane_id, "s"])?;
        let confirmed = wait_for_capture(pane_id, |content| {
            claude_confirmation(content, model).then_some(())
        });
        if let Err(error) = confirmed {
            return Err(UnsupportedModelControl(format!(
                "Claude did not confirm its session-only model change: {error:#}"
            ))
            .into());
        }
        Ok(())
    }

    fn switch_claude_effort(&self, pane_id: &str, effort: &str) -> Result<()> {
        let target = claude_effort_index(effort)?;
        self.send_text(pane_id, "/effort", true)?;
        wait_for_capture(pane_id, |content| {
            content.contains("←/→ to adjust").then_some(())
        })?;
        for _ in 0..5 {
            Self::output(["send-keys", "-t", pane_id, "Left"])?;
        }
        for _ in 0..target {
            Self::output(["send-keys", "-t", pane_id, "Right"])?;
        }
        Self::output(["send-keys", "-t", pane_id, "Enter"])?;
        wait_for_capture(pane_id, |content| {
            content
                .to_ascii_lowercase()
                .contains(&format!("set effort level to {effort}"))
                .then_some(())
        })
        .map_err(|error| {
            UnsupportedModelControl(format!(
                "Claude did not confirm its effort change: {error:#}"
            ))
            .into()
        })
    }

    fn switch_codex_model(
        &self,
        pane_id: &str,
        model: &str,
        desired_effort: Option<&str>,
    ) -> Result<()> {
        let before = self.capture(pane_id, 80).unwrap_or_default();
        let effort = desired_effort
            .map(str::to_owned)
            .or_else(|| codex_effort(&before));
        self.send_text(pane_id, "/model", true)?;
        let menu = wait_for_capture(pane_id, |content| {
            menu_selection(content, "Select Model and Effort", CODEX_MODELS)
        })?;
        let target = model_index(CODEX_MODELS, model)?;
        move_menu_selection(pane_id, menu, target)?;
        Self::output(["send-keys", "-t", pane_id, "Enter"])?;

        let effort_menu = wait_for_capture(pane_id, |content| {
            reason_menu_selection(content, effort.as_deref())
        })?;
        if let ReasonMenu::Move { current, target } = effort_menu {
            move_menu_selection(pane_id, current, target)?;
        }
        Self::output(["send-keys", "-t", pane_id, "Enter"])?;
        let confirmed = wait_for_capture(pane_id, |content| {
            codex_confirmation(content, model).then_some(())
        });
        if let Err(error) = confirmed {
            return Err(UnsupportedModelControl(format!(
                "Codex did not confirm its model change: {error:#}"
            ))
            .into());
        }
        Ok(())
    }

    #[must_use]
    pub fn inside_tmux() -> bool {
        env::var_os("TMUX").is_some()
    }

    #[must_use]
    pub fn current_session(&self) -> Option<String> {
        let pane = env::var("TMUX_PANE").ok()?;
        Self::output(["display-message", "-p", "-t", &pane, "#{session_name}"])
            .ok()
            .map(|value| value.trim().to_owned())
    }

    fn wait_until_session_is_stable(target: &str, name: &str) -> Result<()> {
        const POLL_INTERVAL: Duration = Duration::from_millis(10);
        const STABLE_FOR: Duration = Duration::from_millis(150);
        const TIMEOUT: Duration = Duration::from_secs(1);

        let deadline = Instant::now() + TIMEOUT;
        let mut present_since = None;
        loop {
            if Self::session_target_exists(target)? {
                let present_since = *present_since.get_or_insert_with(Instant::now);
                if present_since.elapsed() >= STABLE_FOR {
                    return Ok(());
                }
            } else {
                bail!("tmux session {name} exited before it became ready");
            }
            if Instant::now() >= deadline {
                bail!("tmux session {name} exited before it became ready");
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn session_target_exists(target: &str) -> Result<bool> {
        let (output, summary) = Self::run(["has-session", "-t", target])?;
        if output.status.success() {
            return Ok(true);
        }
        if is_missing_session_output(&output) {
            return Ok(false);
        }
        check_output(&output, &summary).map(|_| false)
    }

    fn run<const N: usize>(args: [&str; N]) -> Result<(Output, String)> {
        let summary = format!("tmux {}", args.join(" "));
        let output = tmux_command()
            .args(args)
            .output()
            .with_context(|| format!("failed to run {summary}"))?;
        Ok((output, summary))
    }

    fn output<const N: usize>(args: [&str; N]) -> Result<String> {
        let (output, summary) = Self::run(args)?;
        check_output(&output, &summary)
    }
}

/// Models whose picker layout atmux has verified for this installed harness
/// version. Unknown versions intentionally return no controls.
#[must_use]
pub fn known_models(agent: AgentKind, version: &str) -> &'static [KnownModel] {
    match agent {
        AgentKind::Claude
            if matches!(
                version,
                "2.1.224" | "2.1.225" | "2.1.226" | "2.1.232" | "2.1.233"
            ) =>
        {
            CLAUDE_MODELS
        }
        AgentKind::Codex if matches!(version, "0.146.1" | "0.147.0") => CODEX_MODELS,
        AgentKind::Claude | AgentKind::Codex | AgentKind::Other => &[],
    }
}

/// Model ids stay data-only even before they are checked against one pane's
/// owner-reported allowlist.
#[must_use]
pub fn valid_model_id(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 80
        && model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
}

fn valid_mode_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_codex_effort(effort: &str) -> bool {
    matches!(effort, "none" | "low" | "medium" | "high" | "xhigh" | "max")
}

pub(crate) fn valid_claude_effort(effort: &str) -> bool {
    matches!(effort, "low" | "medium" | "high" | "xhigh" | "max")
}

fn active_arguments(arguments: &[String]) -> &[String] {
    &arguments[..arguments
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(arguments.len())]
}

fn insert_before_option_terminator(
    arguments: &mut Vec<String>,
    inserted: impl IntoIterator<Item = String>,
) {
    let index = arguments
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(arguments.len());
    arguments.splice(index..index, inserted);
}

fn is_claude_resume_selector(argument: &str) -> bool {
    matches!(argument, "--resume" | "-r" | "--continue" | "-c") || argument.starts_with("--resume=")
}

/// Normalizes atmux-owned Claude permission arguments for both fresh and
/// reconstructed launches. One dangerous flag and one explicit bypass mode
/// are required because a profile setting such as `defaultMode = "auto"` may
/// otherwise override the flag in current Claude releases. Same-looking values
/// after `--` remain literal data.
fn normalize_claude_permission_arguments(arguments: &mut Vec<String>) {
    let mut index = 0;
    let mut saw_skip = false;
    while index < arguments.len() && arguments[index] != "--" {
        if arguments[index] == CLAUDE_SKIP_PERMISSIONS_FLAG {
            if saw_skip {
                arguments.remove(index);
            } else {
                saw_skip = true;
                index += 1;
            }
            continue;
        }
        if arguments[index] == CLAUDE_PERMISSION_MODE_FLAG {
            arguments.remove(index);
            if index < arguments.len()
                && arguments[index] != "--"
                && !arguments[index].starts_with('-')
            {
                arguments.remove(index);
            }
            continue;
        }
        if arguments[index].starts_with("--permission-mode=") {
            arguments.remove(index);
            continue;
        }
        index += 1;
    }
    if !saw_skip {
        insert_before_option_terminator(arguments, [CLAUDE_SKIP_PERMISSIONS_FLAG.to_owned()]);
    }
    let skip = active_arguments(arguments)
        .iter()
        .position(|argument| argument == CLAUDE_SKIP_PERMISSIONS_FLAG)
        .expect("the normalized Claude permission flag disappeared");
    arguments.insert(skip + 1, CLAUDE_PERMISSION_MODE_FLAG.to_owned());
    arguments.insert(skip + 2, CLAUDE_BYPASS_PERMISSIONS_MODE.to_owned());
}

fn append_native_resume_arguments(
    invocation: &mut Vec<String>,
    argument_start: usize,
    is_claude: bool,
    resume_arguments: Vec<String>,
) {
    if !is_claude {
        invocation.extend(resume_arguments);
        return;
    }
    let mut arguments = invocation.split_off(argument_start);
    insert_before_option_terminator(&mut arguments, resume_arguments);
    normalize_claude_permission_arguments(&mut arguments);
    invocation.extend(arguments);
}

fn build_native_relaunch_invocation(
    exact_profile: &AgentProfile,
    mode: &ProfileMode,
    harness: crate::auto_update::Harness,
    resume_arguments: Vec<String>,
) -> Result<Vec<String>> {
    let mut exact_profile = exact_profile.clone();
    if harness == crate::auto_update::Harness::Claude {
        // Maintenance always replaces a pane with the validated native
        // executable, even when the original saved-launch profile delegated
        // permission handling to an opaque wrapper.
        exact_profile.claude_relaunch_permissions = Some(ClaudeRelaunchPermissions::AtmuxInjects);
    }
    let mut invocation = Tmux::build_invocation(&exact_profile, Some(mode), None)?;
    let argument_start = 2 + exact_profile.env.len();
    append_native_resume_arguments(
        &mut invocation,
        argument_start,
        harness == crate::auto_update::Harness::Claude,
        resume_arguments,
    );
    Ok(invocation)
}

fn claude_resume_invocation(
    claude_program: &Path,
    config_dir: &Path,
    session_id: &str,
) -> Result<Vec<String>> {
    if !valid_claude_resume_session_id(session_id) {
        bail!("the Claude session id is invalid");
    }
    let config_dir = config_dir.to_str().with_context(|| {
        format!(
            "Claude configuration directory is not valid UTF-8: {}",
            config_dir.display()
        )
    })?;
    if config_dir.is_empty() {
        bail!("the Claude configuration directory is empty");
    }
    let claude_program = claude_program.to_str().with_context(|| {
        format!(
            "Claude launcher path is not valid UTF-8: {}",
            claude_program.display()
        )
    })?;
    if !claude_program.starts_with('/') {
        bail!("the Claude launcher path is not absolute");
    }
    Ok(vec![
        "env".to_owned(),
        format!("CLAUDE_CONFIG_DIR={config_dir}"),
        claude_program.to_owned(),
        CLAUDE_SKIP_PERMISSIONS_FLAG.to_owned(),
        CLAUDE_PERMISSION_MODE_FLAG.to_owned(),
        CLAUDE_BYPASS_PERMISSIONS_MODE.to_owned(),
        "--resume".to_owned(),
        session_id.to_owned(),
    ])
}

#[cfg(test)]
fn claude_resume_command(
    claude_program: &Path,
    config_dir: &Path,
    session_id: &str,
) -> Result<String> {
    claude_resume_invocation(claude_program, config_dir, session_id).map(shell_words::join)
}

fn valid_claude_resume_session_id(value: &str) -> bool {
    value.len() == 36
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

fn valid_effort_id(effort: &str) -> bool {
    valid_codex_effort(effort) || valid_claude_effort(effort) || effort == "ultracode"
}

fn observe_model(
    agent: AgentKind,
    content: &str,
    stored: Option<&str>,
    stored_version: Option<&str>,
) -> ModelObservation {
    let version = agent_version(agent, content).or_else(|| stored_version.map(str::to_owned));
    let current = switched_model(agent, content)
        .or_else(|| stored.map(str::to_owned))
        .or_else(|| displayed_model(agent, content));
    ModelObservation {
        version,
        current,
        effort: None,
        mode: None,
    }
}

fn remember_model_metadata(
    pane_id: &str,
    agent: AgentKind,
    content: &str,
    stored_model: &str,
    stored_version: &str,
) {
    if let Some(version) = agent_version(agent, content)
        && version != stored_version
    {
        let _ = Tmux::output([
            "set-option",
            "-p",
            "-t",
            pane_id,
            "@atmux_agent_version",
            &version,
        ]);
    }
    let confirmed = switched_model(agent, content);
    if let Some(current) = confirmed.or_else(|| {
        stored_model
            .is_empty()
            .then(|| displayed_model(agent, content))
            .flatten()
    }) && current != stored_model
    {
        let _ = Tmux::output(["set-option", "-p", "-t", pane_id, "@atmux_model", &current]);
    }
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 24
        && version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

fn agent_version(agent: AgentKind, content: &str) -> Option<String> {
    let marker = match agent {
        AgentKind::Claude => "Claude Code v",
        AgentKind::Codex => "OpenAI Codex (v",
        AgentKind::Other => return None,
    };
    content.lines().find_map(|line| {
        let (_, tail) = line.split_once(marker)?;
        let version = tail
            .chars()
            .take_while(|character| character.is_ascii_digit() || *character == '.')
            .collect::<String>();
        (!version.is_empty()).then_some(version)
    })
}

fn switched_model(agent: AgentKind, content: &str) -> Option<String> {
    content.lines().rev().find_map(|line| match agent {
        AgentKind::Claude if line.contains("Set model to ") => canonical_claude_model(line),
        AgentKind::Codex => line
            .split_once("Model changed to ")
            .and_then(|(_, tail)| tail.split_whitespace().next())
            .filter(|model| valid_model_id(model))
            .map(str::to_owned),
        AgentKind::Claude | AgentKind::Other => None,
    })
}

fn displayed_model(agent: AgentKind, content: &str) -> Option<String> {
    match agent {
        AgentKind::Claude => content.lines().find_map(|line| {
            (line.contains(" with ") || line.contains("model:"))
                .then(|| canonical_claude_model(line))
                .flatten()
        }),
        AgentKind::Codex => content.lines().rev().find_map(|line| {
            let trimmed = line.trim_start_matches([' ', '│', '›', '╰', '╭']);
            let candidate = if let Some((_, tail)) = trimmed.split_once("model:") {
                tail.split_whitespace().next()
            } else if trimmed.starts_with("gpt-") {
                trimmed.split_whitespace().next()
            } else {
                None
            }?;
            valid_model_id(candidate).then(|| candidate.to_owned())
        }),
        AgentKind::Other => None,
    }
}

fn canonical_claude_model(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    if line.contains("Set model to ") && lower.contains("(default)") {
        return Some("default".to_owned());
    }
    ["opus", "fable", "sonnet", "haiku", "default"]
        .into_iter()
        .find(|model| lower.contains(model))
        .map(str::to_owned)
}

fn codex_effort(content: &str) -> Option<String> {
    content.lines().rev().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        if !lower.contains("gpt-") {
            return None;
        }
        ["xhigh", "high", "medium", "low", "max"]
            .into_iter()
            .find(|effort| lower.split_whitespace().any(|word| word == *effort))
            .map(str::to_owned)
    })
}

fn displayed_effort(agent: AgentKind, content: &str) -> Option<String> {
    let values: &[&str] = match agent {
        AgentKind::Claude => &["ultracode", "xhigh", "high", "medium", "low", "max"],
        AgentKind::Codex => &["xhigh", "high", "medium", "low", "max"],
        AgentKind::Other => return None,
    };
    content.lines().rev().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        values.iter().find_map(|effort| {
            let marker = format!("{effort} effort");
            (lower.contains(&marker) || lower.split_whitespace().any(|word| word == *effort))
                .then(|| (*effort).to_owned())
        })
    })
}

fn claude_effort_index(effort: &str) -> Result<usize> {
    ["low", "medium", "high", "xhigh", "max"]
        .iter()
        .position(|candidate| *candidate == effort)
        .ok_or_else(|| {
            UnsupportedModelControl(format!("unsupported Claude effort {effort}")).into()
        })
}

fn model_index(models: &[KnownModel], model: &str) -> Result<usize> {
    models
        .iter()
        .position(|candidate| candidate.id == model)
        .ok_or_else(|| UnsupportedModelControl(format!("unknown model {model}")).into())
}

fn menu_selection(content: &str, heading: &str, models: &[KnownModel]) -> Option<usize> {
    if !content.contains(heading) {
        return None;
    }
    let mut selected = None;
    for (index, model) in models.iter().enumerate() {
        let number = format!("{}. ", index + 1);
        let line = content.lines().find(|line| {
            line.contains(&number)
                && line
                    .to_ascii_lowercase()
                    .contains(&model.id.to_ascii_lowercase())
        })?;
        if line.trim_start().starts_with(['❯', '›']) {
            selected = Some(index);
        }
    }
    selected
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReasonMenu {
    KeepDefault,
    Move { current: usize, target: usize },
}

fn reason_menu_selection(content: &str, desired: Option<&str>) -> Option<ReasonMenu> {
    if !content.contains("Select Reasoning Level for ") {
        return None;
    }
    let rows = ["Low", "Medium", "High", "Extra high", "More reasoning"];
    let mut current = None;
    let mut present = Vec::new();
    for (index, label) in rows.iter().enumerate() {
        let number = format!("{}. ", index + 1);
        if let Some(line) = content
            .lines()
            .find(|line| line.contains(&number) && line.contains(label))
        {
            present.push(index);
            if line.trim_start().starts_with(['❯', '›']) {
                current = Some(index);
            }
        }
    }
    let current = current?;
    let target = match desired {
        Some("low") => 0,
        Some("medium") => 1,
        Some("high") => 2,
        Some("xhigh") => 3,
        Some("max") => 4,
        _ => return Some(ReasonMenu::KeepDefault),
    };
    Some(if present.contains(&target) {
        ReasonMenu::Move { current, target }
    } else {
        ReasonMenu::KeepDefault
    })
}

fn move_menu_selection(pane_id: &str, current: usize, target: usize) -> Result<()> {
    let (key, count) = if target >= current {
        ("Down", target - current)
    } else {
        ("Up", current - target)
    };
    for _ in 0..count {
        Tmux::output(["send-keys", "-t", pane_id, key])?;
    }
    Ok(())
}

fn wait_for_capture<T>(pane_id: &str, mut parse: impl FnMut(&str) -> Option<T>) -> Result<T> {
    let deadline = Instant::now() + MODEL_MENU_TIMEOUT;
    loop {
        let content = Tmux.capture(pane_id, 120)?;
        if let Some(value) = parse(&content) {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(UnsupportedModelControl(
                "the running agent did not expose the expected model picker".to_owned(),
            )
            .into());
        }
        thread::sleep(MODEL_MENU_POLL);
    }
}

fn claude_confirmation(content: &str, model: &str) -> bool {
    content.lines().rev().take(20).any(|line| {
        line.contains("Set model to ")
            && line.contains("for this session only")
            && canonical_claude_model(line).as_deref() == Some(model)
    })
}

fn codex_confirmation(content: &str, model: &str) -> bool {
    content.lines().rev().take(20).any(|line| {
        line.split_once("Model changed to ")
            .and_then(|(_, tail)| tail.split_whitespace().next())
            == Some(model)
    })
}

fn output_stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .trim()
        .to_lowercase()
}

fn is_missing_server_output(output: &Output) -> bool {
    is_missing_server_message(&output_stderr(output))
}

fn is_missing_server_message(stderr: &str) -> bool {
    let stderr = stderr.to_lowercase();
    stderr.contains("no server running")
        || ((stderr.contains("error connecting to ")
            || stderr.contains("failed to connect to server"))
            && (stderr.contains("no such file or directory")
                || stderr.contains("connection refused")))
}

fn is_missing_session_output(output: &Output) -> bool {
    let stderr = output_stderr(output);
    is_missing_server_message(&stderr)
        || stderr.contains("can't find session")
        || stderr.contains("no such session")
}

fn paste_buffer_args<'a>(buffer: &'a str, pane_id: &'a str) -> [&'a str; 8] {
    [
        "paste-buffer",
        "-d",
        "-p",
        "-r",
        "-b",
        buffer,
        "-t",
        pane_id,
    ]
}

fn auto_compact_delivery_args(pane_id: &str) -> [&str; 10] {
    [
        "send-keys",
        "-t",
        pane_id,
        "-l",
        "/compact",
        ";",
        "send-keys",
        "-t",
        pane_id,
        "Enter",
    ]
}

fn respawn_pane_args<'a>(pane_id: &'a str, directory: &'a str, command: &'a str) -> [&'a str; 7] {
    [
        "respawn-pane",
        "-k",
        "-t",
        pane_id,
        "-c",
        directory,
        command,
    ]
}

fn submission_keys(submit: bool) -> &'static [&'static str] {
    if submit { &["Enter"] } else { &[] }
}

fn submit_args(pane_id: &str) -> [&str; 4] {
    ["send-keys", "-t", pane_id, "Enter"]
}

fn special_key_args(pane_id: &str, key: PaneSpecialKey) -> [&str; 4] {
    let key = match key {
        PaneSpecialKey::Up => "Up",
        PaneSpecialKey::Down => "Down",
        PaneSpecialKey::Left => "Left",
        PaneSpecialKey::Right => "Right",
        PaneSpecialKey::Enter => "Enter",
        PaneSpecialKey::TmuxPrefixTwice => {
            unreachable!("tmux prefix uses its fixed two-key command path")
        }
    };
    ["send-keys", "-t", pane_id, key]
}

fn tmux_prefix_twice_keys() -> &'static [&'static str] {
    &["C-b", "C-b"]
}

fn select_session_panes(
    source: &str,
    process_table: &ProcessTable,
) -> HashMap<String, (RawPane, AgentKind)> {
    let mut selected = HashMap::new();
    for line in source.lines().filter(|line| !line.trim().is_empty()) {
        let Some(pane) = parse_pane(line) else {
            continue;
        };
        if pane.name == RESERVED_SERVICE_SESSION {
            continue;
        }
        let process_tree = process_table.commands_under(pane.pane_pid);
        // `pane_current_command` becomes the numeric Claude version for
        // current releases. Include the launch command as well: it retains
        // the versioned executable path and the profile's
        // `CLAUDE_CONFIG_DIR`, both of which safely identify the harness.
        let detection_context = format!("{process_tree} {}", pane.start_command);
        let agent = status::detect_kind(&pane.command, &detection_context);
        let should_replace = selected
            .get(&pane.name)
            .is_none_or(|(existing, existing_agent)| {
                pane_rank(&pane, agent) > pane_rank(existing, *existing_agent)
            });
        if should_replace {
            selected.insert(pane.name.clone(), (pane, agent));
        }
    }
    selected
}

fn check_output(output: &Output, summary: &str) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if stderr.is_empty() {
            bail!("{summary} exited with {}", output.status);
        }
        bail!("{summary}: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}

fn parse_pane(line: &str) -> Option<RawPane> {
    let fields: Vec<_> = line.split('\t').collect();
    if fields.len() < 15 {
        return None;
    }
    Some(RawPane {
        name: fields[0].to_owned(),
        attached: fields[1] == "1",
        windows: fields[2].parse().ok()?,
        activity: fields[3].parse().unwrap_or_default(),
        window_index: fields[4].parse().unwrap_or_default(),
        window_active: fields[5] == "1",
        pane_index: fields[6].parse().unwrap_or_default(),
        pane_active: fields[7] == "1",
        pane_pid: fields[8].parse().ok()?,
        command: fields[9].to_owned(),
        start_command: fields[10].to_owned(),
        path: PathBuf::from(fields[11]),
        title: fields[12].to_owned(),
        pane_id: fields[13].to_owned(),
        status_override: fields[14].to_owned(),
        model_override: fields.get(15).copied().unwrap_or_default().to_owned(),
        agent_version: fields.get(16).copied().unwrap_or_default().to_owned(),
        profile: fields.get(17).copied().unwrap_or_default().to_owned(),
        resume_lease: fields.get(18).copied().unwrap_or_default().to_owned(),
        pane_identity: fields.get(19).copied().unwrap_or_default().to_owned(),
        systemd_scope: fields.get(20).copied().unwrap_or_default().to_owned(),
        memory_max_bytes: fields.get(21).copied().unwrap_or_default().to_owned(),
    })
}

fn scope_metadata(scope: &str, memory_max_bytes: &str) -> (Option<String>, Option<u64>) {
    let memory_max_bytes = memory_max_bytes
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0);
    if systemd_scope::valid_scope_name(scope) && memory_max_bytes.is_some() {
        (Some(scope.to_owned()), memory_max_bytes)
    } else {
        (None, None)
    }
}

fn publish_scope_metadata(pane_id: &str, scope: &PreparedScope) -> Result<()> {
    match scope.metadata() {
        Some((unit, memory_max_bytes)) => Tmux::output([
            "set-option",
            "-p",
            "-t",
            pane_id,
            "@atmux_systemd_scope",
            unit,
            ";",
            "set-option",
            "-p",
            "-t",
            pane_id,
            "@atmux_memory_max_bytes",
            &memory_max_bytes.to_string(),
        ]),
        None => Tmux::output([
            "set-option",
            "-p",
            "-u",
            "-t",
            pane_id,
            "@atmux_systemd_scope",
            ";",
            "set-option",
            "-p",
            "-u",
            "-t",
            pane_id,
            "@atmux_memory_max_bytes",
        ]),
    }
    .map(|_| ())
}

pub(crate) fn valid_pane_identity(value: &str) -> bool {
    value.strip_prefix("pane-v1-").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn ensure_pane_identity(pane_id: &str, observed: &str) -> Result<String> {
    if valid_pane_identity(observed) {
        return Ok(observed.to_owned());
    }
    if !observed.is_empty() {
        bail!("tmux pane has an invalid atmux identity");
    }
    let sequence = PANE_IDENTITY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let digest =
        Sha256::digest(format!("{}:{now}:{sequence}:{pane_id}", std::process::id()).as_bytes());
    let generated = format!("pane-v1-{digest:x}");
    // `-o` is set-if-absent. If another owner refresh won the race, the
    // command fails harmlessly and the authoritative pane option is read.
    let _ = Tmux::output([
        "set-option",
        "-p",
        "-o",
        "-t",
        pane_id,
        "@atmux_identity",
        &generated,
    ]);
    let identity = Tmux::output(["display-message", "-p", "-t", pane_id, "#{@atmux_identity}"])?;
    if !valid_pane_identity(&identity) {
        bail!("tmux pane identity could not be established");
    }
    Ok(identity)
}

fn valid_resume_lease(value: &str) -> bool {
    value.strip_prefix("lease-v1-").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn valid_tmux_session_id(value: &str) -> bool {
    value
        .strip_prefix('$')
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
}

fn valid_tmux_pane_id(value: &str) -> bool {
    value
        .strip_prefix('%')
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
}

fn parse_tmux_created_target(value: &str) -> Option<(String, String)> {
    let (session, pane) = value.split_once('\t')?;
    (valid_tmux_session_id(session) && valid_tmux_pane_id(pane))
        .then(|| (session.to_owned(), pane.to_owned()))
}

fn agent_profile_label(start_command: &str, explicit: &str, agent: AgentKind) -> String {
    if !explicit.is_empty()
        && explicit.len() <= 128
        && explicit
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return explicit.to_owned();
    }
    let descriptor = launch_command_label(start_command);
    if let Some((configuration, _)) = descriptor.split_once(" · ")
        && let Some((key, directory)) = configuration.split_once('=')
        && matches!(key, "CLAUDE_CONFIG_DIR" | "CODEX_HOME")
        && let Some(leaf) = directory
            .split_whitespace()
            .next()
            .and_then(|value| value.rsplit('/').next())
    {
        let conventional = leaf.trim_start_matches('.');
        if conventional == "claude" || conventional == "codex" {
            return "Default".to_owned();
        }
        if !conventional.is_empty()
            && conventional.len() <= 128
            && conventional
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return conventional.to_owned();
        }
    }
    if let Some(executable) = descriptor.rsplit(" · ").next()
        && let Some(leaf) = Path::new(executable)
            .file_name()
            .and_then(|name| name.to_str())
        && ((agent == AgentKind::Claude && leaf.starts_with("claude-"))
            || (agent == AgentKind::Codex && leaf.starts_with("codex-")))
        && leaf.len() <= 128
        && leaf
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return leaf.to_owned();
    }
    match agent {
        AgentKind::Claude | AgentKind::Codex => "Default".to_owned(),
        AgentKind::Other => String::new(),
    }
}

/// Produces a safe launch descriptor for display in the dashboard.
///
/// Tmux's full start command may contain user prompts, arbitrary arguments, or
/// environment secrets. The dashboard therefore exposes only the executable
/// and the non-sensitive conventional Claude/Codex configuration directory.
fn launch_command_label(start_command: &str) -> String {
    let mut words = shell_words::split(start_command).unwrap_or_else(|_| {
        start_command
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    });
    if words.len() == 1
        && words[0].chars().any(char::is_whitespace)
        && let Ok(inner) = shell_words::split(&words[0])
    {
        words = inner;
    }
    if let Some(agent_words) = systemd_scope::agent_argv(&words) {
        words = agent_words;
    }

    let mut executable = None;
    let mut config_directory = None;
    let mut expect_command = true;
    let mut skip_next = false;
    let mut env_mode = false;
    for word in words {
        if matches!(word.as_str(), "&&" | "||" | "|" | ";") {
            executable = None;
            expect_command = true;
            env_mode = false;
            continue;
        }
        if skip_next {
            skip_next = false;
            continue;
        }
        if word == "cd" {
            skip_next = true;
            continue;
        }
        if matches!(word.as_str(), "exec" | "command") {
            expect_command = true;
            continue;
        }
        if word == "env" {
            expect_command = true;
            env_mode = true;
            continue;
        }
        if env_mode && matches!(word.as_str(), "-u" | "--unset") {
            skip_next = true;
            continue;
        }
        if env_mode && word.starts_with('-') {
            continue;
        }
        if let Some((key, value)) = word.split_once('=') {
            if matches!(key, "CLAUDE_CONFIG_DIR" | "CODEX_HOME") {
                config_directory = safe_agent_config_directory(key, value)
                    .map(|directory| (key.to_owned(), directory));
            }
            continue;
        }
        if expect_command && !word.starts_with('-') {
            executable = Some(word);
            expect_command = false;
            env_mode = false;
        }
    }

    let Some(executable) = executable else {
        return String::new();
    };
    config_directory.map_or(executable.clone(), |(key, directory)| {
        format!("{key}={directory} · {executable}")
    })
}

/// Preserves a conventional local Claude configuration path without exposing
/// an arbitrary environment value. A literal tilde is marked explicitly: tmux
/// receives quoted `env` assignments verbatim, so it would not be expanded.
fn safe_agent_config_directory(key: &str, value: &str) -> Option<String> {
    let leaf = value.rsplit('/').next().unwrap_or(value);
    let prefix = if key == "CLAUDE_CONFIG_DIR" {
        ".claude"
    } else {
        ".codex"
    };
    (leaf.starts_with(prefix)
        && leaf
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
    .then(|| {
        if value.starts_with("~/") {
            format!("{value} (unexpanded)")
        } else {
            value.to_owned()
        }
    })
}

fn hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn command_available(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return is_executable(path);
    }
    env::var_os("PATH").is_some_and(|path_value| {
        env::split_paths(&path_value).any(|directory| is_executable(&directory.join(command)))
    })
}

/// Finds tmux even when a macOS GUI launcher starts atmux with a minimal PATH.
///
/// Homebrew installs on Apple Silicon under `/opt/homebrew`; Intel Homebrew
/// commonly uses `/usr/local`. Both are trusted fixed locations and are used
/// only after the normal PATH lookup fails.
fn tmux_program() -> PathBuf {
    if command_available("tmux") {
        return PathBuf::from("tmux");
    }
    #[cfg(target_os = "macos")]
    for candidate in ["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux"] {
        let path = PathBuf::from(candidate);
        if is_executable(&path) {
            return path;
        }
    }
    PathBuf::from("tmux")
}

fn tmux_command() -> Command {
    let mut command = Command::new(tmux_program());
    let socket = TMUX_SOCKET_OVERRIDE
        .with(|current| current.borrow().clone())
        .or_else(|| {
            env::var("ATMUX_TMUX_SOCKET_NAME")
                .ok()
                .filter(|socket| valid_socket_name(socket))
        });
    if let Some(socket) = socket {
        command.args(["-L", &socket]);
    }
    command
}

fn valid_socket_name(socket: &str) -> bool {
    !socket.is_empty()
        && socket.len() <= 64
        && socket
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn popup_attach_command(name: &str, tmux_environment: &str) -> Result<String> {
    let socket = tmux_environment
        .split(',')
        .next()
        .filter(|value| !value.is_empty())
        .context("TMUX does not contain a server socket")?;
    Ok(shell_words::join([
        "env",
        "-u",
        "TMUX",
        "-u",
        "TMUX_PANE",
        tmux_program().to_string_lossy().as_ref(),
        "-S",
        socket,
        "attach-session",
        "-t",
        name,
    ]))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[derive(Debug, Default)]
struct ProcessTable {
    entries: HashMap<u32, (u32, Option<Duration>, String)>,
}

impl ProcessTable {
    fn load() -> Self {
        let Ok(output) = Command::new("ps")
            .args(["-axo", "pid=,ppid=,etime=,command="])
            .output()
        else {
            return Self::default();
        };
        let source = String::from_utf8_lossy(&output.stdout);
        let mut entries = HashMap::new();
        for line in source.lines() {
            let mut fields = line.split_whitespace();
            let (Some(pid), Some(parent), Some(elapsed)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let (Ok(pid), Ok(parent)) = (pid.parse(), parent.parse()) else {
                continue;
            };
            entries.insert(
                pid,
                (
                    parent,
                    parse_elapsed(elapsed),
                    fields.collect::<Vec<_>>().join(" "),
                ),
            );
        }
        Self { entries }
    }

    fn commands_under(&self, root: u32) -> String {
        self.entries
            .iter()
            .filter_map(|(&pid, (_, _, command))| self.descends_from(pid, root).then_some(command))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn descends_from(&self, mut pid: u32, root: u32) -> bool {
        for _ in 0..64 {
            if pid == root {
                return true;
            }
            let Some((parent, _, _)) = self.entries.get(&pid) else {
                return false;
            };
            if *parent == 0 || *parent == pid {
                return false;
            }
            pid = *parent;
        }
        false
    }

    fn agent_process_under(&self, root: u32, kind: AgentKind) -> Option<(u32, Option<u64>)> {
        self.agent_process_under_ranked(root, kind, agent_process_rank)
    }

    fn agent_process_under_ranked(
        &self,
        root: u32,
        kind: AgentKind,
        mut rank_process: impl FnMut(&str, AgentKind) -> u8,
    ) -> Option<(u32, Option<u64>)> {
        let (pid, (_, elapsed, _), _) = self
            .entries
            .iter()
            .filter_map(|(pid, entry)| {
                let rank = rank_process(&entry.2, kind);
                (self.descends_from(*pid, root) && rank > 0).then_some((*pid, entry, rank))
            })
            .max_by_key(|(pid, _, rank)| (*rank, self.depth_from(*pid, root)))?;
        let started = elapsed.and_then(|elapsed| {
            SystemTime::now()
                .checked_sub(elapsed)?
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        });
        Some((pid, started))
    }

    fn depth_from(&self, mut pid: u32, root: u32) -> usize {
        for depth in 0..64 {
            if pid == root {
                return depth;
            }
            let Some((parent, _, _)) = self.entries.get(&pid) else {
                break;
            };
            pid = *parent;
        }
        usize::MAX
    }
}

fn parse_elapsed(value: &str) -> Option<Duration> {
    let (days, clock) = if let Some((days, clock)) = value.split_once('-') {
        (days.parse::<u64>().ok()?, clock)
    } else {
        (0_u64, value)
    };
    let fields = clock
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let seconds = match fields.as_slice() {
        [minutes, seconds] => minutes.checked_mul(60)?.checked_add(*seconds)?,
        [hours, minutes, seconds] => hours
            .checked_mul(3_600)?
            .checked_add(minutes.checked_mul(60)?)?
            .checked_add(*seconds)?,
        _ => return None,
    };
    Some(Duration::from_secs(
        days.checked_mul(86_400)?.checked_add(seconds)?,
    ))
}

#[cfg(test)]
fn command_is_agent_process(command: &str, kind: AgentKind) -> bool {
    agent_process_rank(command, kind) > 0
}

fn agent_process_rank(command: &str, kind: AgentKind) -> u8 {
    let home = env::var_os("HOME").map(PathBuf::from);
    agent_process_rank_in(
        command,
        kind,
        home.as_deref(),
        rustix::process::geteuid().as_raw(),
    )
}

fn agent_process_rank_in(
    command: &str,
    kind: AgentKind,
    home: Option<&Path>,
    expected_uid: u32,
) -> u8 {
    let words = shell_words::split(command)
        .unwrap_or_else(|_| command.split_whitespace().map(str::to_owned).collect());
    let executable_path = words.first().map(String::as_str).unwrap_or_default();
    let executable = Path::new(executable_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match kind {
        // Native Claude installs exec a version-named binary directly.  This
        // is common for restored panes and macOS LaunchAgent sessions, where
        // argv[0] is e.g. ~/.local/share/claude/versions/2.1.238 rather than
        // `claude`. Keep the match pinned to this user's canonical install
        // layout so an unrelated version-looking descendant cannot acquire a
        // pane's transcript identity.
        AgentKind::Claude
            if home.is_some_and(|home| {
                native_claude_version_executable_in(executable_path, home, expected_uid)
            }) =>
        {
            3
        }
        AgentKind::Claude if executable == "claude" => 2,
        AgentKind::Claude if executable.starts_with("claude-") => 1,
        AgentKind::Codex => {
            if executable == "codex" {
                2
            } else {
                u8::from(
                    (executable == "node" || executable.starts_with("node"))
                        && words.iter().skip(1).any(|word| {
                            Path::new(word).file_name().and_then(|name| name.to_str())
                                == Some("codex")
                        }),
                )
            }
        }
        AgentKind::Claude | AgentKind::Other => 0,
    }
}

fn native_claude_version_executable_in(executable: &str, home: &Path, expected_uid: u32) -> bool {
    let path = Path::new(executable);
    let Some(version) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let valid_version = version.split('.').count() == 3
        && version.split('.').all(|part| {
            !part.is_empty() && part.len() <= 6 && part.bytes().all(|byte| byte.is_ascii_digit())
        });
    if !path.is_absolute() || !home.is_absolute() || !valid_version {
        return false;
    }

    let local = home.join(".local");
    let share = local.join("share");
    let claude = share.join("claude");
    let versions = claude.join("versions");
    let expected = versions.join(version);
    if path != expected {
        return false;
    }

    for (component, expect_directory) in [
        (home, true),
        (local.as_path(), true),
        (share.as_path(), true),
        (claude.as_path(), true),
        (versions.as_path(), true),
        (expected.as_path(), false),
    ] {
        let Ok(metadata) = fs::symlink_metadata(component) else {
            return false;
        };
        if metadata.file_type().is_symlink()
            || metadata.uid() != expected_uid
            || (expect_directory && !metadata.is_dir())
            || (!expect_directory
                && (!metadata.is_file() || metadata.permissions().mode() & 0o111 == 0))
        {
            return false;
        }
    }

    let (Ok(canonical_home), Ok(canonical_versions), Ok(canonical_executable)) = (
        home.canonicalize(),
        versions.canonicalize(),
        expected.canonicalize(),
    ) else {
        return false;
    };
    canonical_versions == canonical_home.join(".local/share/claude/versions")
        && canonical_executable == canonical_versions.join(version)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::{FileTypeExt as _, symlink};

    use super::*;

    struct DisposableTmux {
        socket: String,
        socket_path: PathBuf,
        directory: PathBuf,
    }

    struct NativeClaudeFixture {
        root: PathBuf,
        home: PathBuf,
        versions: PathBuf,
        executable: PathBuf,
    }

    impl NativeClaudeFixture {
        fn new(name: &str, version: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = env::temp_dir().join(format!(
                "atmux-native-claude-{name}-{}-{nonce}",
                std::process::id()
            ));
            let home = root.join("home");
            let versions = home.join(".local/share/claude/versions");
            fs::create_dir_all(&versions).unwrap();
            let executable = versions.join(version);
            fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                root,
                home,
                versions,
                executable,
            }
        }
    }

    impl Drop for NativeClaudeFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    impl Drop for DisposableTmux {
        fn drop(&mut self) {
            let _ = Tmux::with_socket_for_test(&self.socket, || {
                let _ = Tmux::output(["kill-server"]);
                Ok(())
            });
            if fs::symlink_metadata(&self.socket_path)
                .is_ok_and(|metadata| metadata.file_type().is_socket())
            {
                let _ = fs::remove_file(&self.socket_path);
            }
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn disposable_tmux(label: &str) -> DisposableTmux {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket = format!("atmux-test-{label}-{}-{nonce}", std::process::id());
        let socket_base = env::var_os("TMUX_TMPDIR").map_or_else(env::temp_dir, PathBuf::from);
        DisposableTmux {
            socket_path: socket_base
                .join(format!("tmux-{}", rustix::process::geteuid().as_raw()))
                .join(&socket),
            directory: env::temp_dir().join(format!(
                "atmux-test-{label}-agent-{}-{nonce}",
                std::process::id()
            )),
            socket,
        }
    }

    #[cfg(target_os = "linux")]
    fn fixed_user_systemctl(arguments: &[&str]) -> Output {
        let uid = rustix::process::geteuid().as_raw();
        Command::new("/usr/bin/env")
            .arg(format!("XDG_RUNTIME_DIR=/run/user/{uid}"))
            .arg(format!(
                "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus"
            ))
            .arg("/usr/bin/systemctl")
            .args(arguments)
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .output()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    struct TestScopeCleanup(Option<String>);

    #[cfg(target_os = "linux")]
    impl Drop for TestScopeCleanup {
        fn drop(&mut self) {
            if let Some(unit) = self.0.as_deref() {
                let _ = fixed_user_systemctl(&["--user", "stop", unit]);
            }
        }
    }

    #[test]
    fn parses_tmux_pane() {
        let line =
            "work\t1\t2\t123\t0\t1\t1\t1\t42\tnode\tenv codex\t/tmp/work\t⠹ work\t%7\twaiting";
        let pane = parse_pane(line).unwrap();
        assert_eq!(pane.name, "work");
        assert_eq!(pane.pane_id, "%7");
        assert_eq!(pane.start_command, "env codex");
        assert_eq!(pane.status_override, "waiting");
        assert_eq!(pane.score(), 3);
    }

    #[test]
    fn parses_pane_with_empty_status_override() {
        let line = "solo\t0\t1\t123\t0\t1\t0\t1\t42\tbash\t\t/tmp\tsolo\t%0\t";
        let pane = parse_pane(line).unwrap();
        assert_eq!(pane.name, "solo");
        assert!(pane.status_override.is_empty());
    }

    #[test]
    fn parses_only_well_formed_persistent_resume_leases() {
        let lease = format!("lease-v1-{}", "a".repeat(64));
        let line = format!(
            "solo\t0\t1\t123\t0\t1\t0\t1\t42\tcodex\tcodex\t/tmp\tsolo\t%0\t\t\t\tDefault\t{lease}"
        );
        let pane = parse_pane(&line).unwrap();
        assert!(valid_resume_lease(&pane.resume_lease));
        assert!(!valid_resume_lease("lease-v1-not-a-digest"));
        assert!(!valid_resume_lease(&format!("lease-v1-{}", "A".repeat(64))));
    }

    #[test]
    fn parses_scope_metadata_only_as_a_valid_complete_pair() {
        let unit = "atmux-tmux-spawn-12-34-0123456789abcdef.scope";
        let line = format!(
            "solo\t0\t1\t123\t0\t1\t0\t1\t42\tcodex\tcodex\t/tmp\tsolo\t%0\t\t\t\tDefault\t\t\t{unit}\t34359738368"
        );
        let pane = parse_pane(&line).unwrap();
        assert_eq!(
            scope_metadata(&pane.systemd_scope, &pane.memory_max_bytes),
            (Some(unit.to_owned()), Some(34_359_738_368))
        );
        assert_eq!(
            scope_metadata("bad;scope.scope", "34359738368"),
            (None, None)
        );
        assert_eq!(scope_metadata(unit, "0"), (None, None));
        assert_eq!(scope_metadata(unit, "not-a-number"), (None, None));
    }

    #[test]
    fn launch_discovers_captures_submits_and_kills_on_an_isolated_server() {
        let probe = disposable_tmux("launch");
        fs::create_dir(&probe.directory).unwrap();
        let command = probe.directory.join("codex");
        fs::write(
            &command,
            concat!(
                "#!/bin/sh\n",
                "printf 'atmux-smoke-ready\\n'\n",
                "IFS= read -r first\n",
                "IFS= read -r second\n",
                "printf 'received:%s|%s\\n' \"$first\" \"$second\"\n",
                "sleep 10\n",
            ),
        )
        .unwrap();
        fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).unwrap();
        let profile = AgentProfile {
            name: "Launch smoke".to_owned(),
            harness: "codex".to_owned(),
            command: command.to_string_lossy().into_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };

        Tmux::with_socket_for_test(&probe.socket, || {
            Tmux::launch(
                "agent",
                &probe.directory,
                &profile,
                None,
                systemd_scope::prepare(&AgentResourcesConfig::default(), "launch-smoke")?,
            )?;
            let sessions = Tmux.sessions(&HashMap::new(), &StatusConfig::default())?;
            let session = sessions
                .iter()
                .find(|session| session.name == "agent")
                .context("launched session was not discovered")?;
            let identity = Tmux::output([
                "show-options",
                "-p",
                "-q",
                "-v",
                "-t",
                &session.pane_id,
                "@atmux_identity",
            ])?;
            assert!(valid_pane_identity(identity.trim()));
            let ready = wait_for_capture(&session.pane_id, |content| {
                content
                    .contains("atmux-smoke-ready")
                    .then(|| content.to_owned())
            })?;
            assert!(ready.contains("atmux-smoke-ready"));
            Tmux.send_text(
                &session.pane_id,
                "hello from atmux\nsecond literal line",
                true,
            )?;
            let submitted = wait_for_capture(&session.pane_id, |content| {
                content
                    .contains("received:hello from atmux|second literal line")
                    .then(|| content.to_owned())
            })?;
            assert!(submitted.contains("received:hello from atmux|second literal line"));
            Tmux.kill("agent")
        })
        .unwrap();
    }

    #[test]
    fn launch_reports_an_immediately_exiting_command() {
        let probe = disposable_tmux("exit");
        fs::create_dir(&probe.directory).unwrap();
        let profile = AgentProfile {
            name: "Immediate exit".to_owned(),
            harness: "codex".to_owned(),
            command: "/bin/sh".to_owned(),
            args: vec!["-lc".to_owned(), "exit 7".to_owned()],
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };
        let error = Tmux::with_socket_for_test(&probe.socket, || {
            Tmux::launch(
                "agent",
                &probe.directory,
                &profile,
                None,
                systemd_scope::prepare(&AgentResourcesConfig::default(), "exit-smoke")?,
            )
        })
        .expect_err("an immediately exiting command must not be reported as launched");
        assert!(
            error.to_string().contains("exited before it became ready"),
            "unexpected launch error: {error:#}"
        );
    }

    /// Real transport proof for the fail-closed path. The disposable named
    /// socket is intentionally unrelated to the user's protected default tmux
    /// server. The test starts with no ambient bus variables, removes them
    /// from the isolated tmux global environment, and verifies that the fixed
    /// wrapper still creates a bounded process-tree scope with working PTY IO.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires tmux and an active systemd user manager"]
    #[allow(clippy::too_many_lines)] // One linear real-host assertion sequence.
    fn scoped_launch_uses_fixed_bus_preserves_pty_and_cleans_up() {
        let probe = disposable_tmux("systemd-scope");
        fs::create_dir(&probe.directory).unwrap();
        let command = probe.directory.join("codex");
        fs::write(
            &command,
            concat!(
                "#!/bin/sh\n",
                "if test -t 0 && test -t 1; then echo pty=yes; else echo pty=no; fi\n",
                "printf 'xdg=%s\\n' \"$XDG_RUNTIME_DIR\"\n",
                "printf 'dbus=%s\\n' \"$DBUS_SESSION_BUS_ADDRESS\"\n",
                "printf 'agent-pid=%s\\n' \"$$\"\n",
                "cat /proc/self/cgroup\n",
                "IFS= read -r input\n",
                "printf 'received=%s\\n' \"$input\"\n",
                "sleep 30\n",
            ),
        )
        .unwrap();
        fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).unwrap();
        let profile = AgentProfile {
            name: "Bounded launch".to_owned(),
            harness: "codex".to_owned(),
            command: command.to_string_lossy().into_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };
        let resources = AgentResourcesConfig {
            memory_max_bytes: Some(64 * 1024 * 1024),
            memory_override_max_bytes: Some(systemd_scope::GIBIBYTE),
        };

        let initial_server = Command::new("tmux")
            .args([
                "-L",
                &probe.socket,
                "new-session",
                "-d",
                "-s",
                "cleanup-canary",
                "/bin/sleep 2147483647",
            ])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .status()
            .unwrap();
        assert!(initial_server.success());

        Tmux::with_socket_for_test(&probe.socket, || {
            Tmux::output(["set-environment", "-gu", "XDG_RUNTIME_DIR"])?;
            Tmux::output(["set-environment", "-gu", "DBUS_SESSION_BUS_ADDRESS"])?;
            for variable in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
                let (output, _) = Tmux::run(["show-environment", "-g", variable])?;
                assert!(!output.status.success(), "tmux retained {variable}");
            }

            let scope = systemd_scope::prepare_override(
                &resources,
                Some(systemd_scope::GIBIBYTE),
                "isolated-real-tmux",
            )?;
            let unit = scope
                .metadata()
                .map(|(unit, _)| unit.to_owned())
                .context("configured policy produced no scope unit")?;
            let mut scope_cleanup = TestScopeCleanup(Some(unit.clone()));
            Tmux::launch("bounded", &probe.directory, &profile, None, scope)?;

            let sessions = Tmux.sessions(&HashMap::new(), &StatusConfig::default())?;
            let session = sessions
                .iter()
                .find(|session| session.name == "bounded")
                .context("bounded session was not discovered")?;
            assert_eq!(session.systemd_scope.as_deref(), Some(unit.as_str()));
            assert_eq!(session.memory_max_bytes, Some(systemd_scope::GIBIBYTE));

            let capture = wait_for_capture(&session.pane_id, |content| {
                content.contains("agent-pid=").then(|| content.to_owned())
            })?;
            assert!(capture.contains("pty=yes"), "{capture}");
            let uid = rustix::process::geteuid().as_raw();
            assert!(
                capture.contains(&format!("xdg=/run/user/{uid}")),
                "{capture}"
            );
            assert!(
                capture.contains(&format!("dbus=unix:path=/run/user/{uid}/bus")),
                "{capture}"
            );
            let agent_pid = capture
                .lines()
                .find_map(|line| line.trim().strip_prefix("agent-pid="))
                .context("capture omitted the scoped agent pid")?;
            let agent_pid = agent_pid.parse::<u32>()?;

            let memory =
                fixed_user_systemctl(&["--user", "show", &unit, "--property=MemoryMax", "--value"]);
            assert!(memory.status.success(), "{:?}", memory.stderr);
            assert_eq!(String::from_utf8(memory.stdout)?.trim(), "1073741824");
            let control_group = fixed_user_systemctl(&[
                "--user",
                "show",
                &unit,
                "--property=ControlGroup",
                "--value",
            ]);
            assert!(control_group.status.success(), "{:?}", control_group.stderr);
            let control_group = String::from_utf8(control_group.stdout)?;
            let processes = fs::read_to_string(
                Path::new("/sys/fs/cgroup")
                    .join(control_group.trim().trim_start_matches('/'))
                    .join("cgroup.procs"),
            )?;
            assert!(
                processes
                    .lines()
                    .any(|process| process.parse::<u32>().ok() == Some(agent_pid)),
                "agent {agent_pid} was outside {unit}: {processes}"
            );

            Tmux.send_text(&session.pane_id, "pty round trip", true)?;
            let submitted = wait_for_capture(&session.pane_id, |content| {
                content
                    .contains("received=pty round trip")
                    .then(|| content.to_owned())
            })?;
            assert!(submitted.contains("received=pty round trip"));

            Tmux.kill("bounded")?;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let loaded = fixed_user_systemctl(&[
                    "--user",
                    "show",
                    &unit,
                    "--property=LoadState",
                    "--value",
                ]);
                assert!(loaded.status.success(), "{:?}", loaded.stderr);
                let loaded = String::from_utf8_lossy(&loaded.stdout);
                if loaded.trim().is_empty() || loaded.trim() == "not-found" {
                    break;
                }
                if Instant::now() >= deadline {
                    bail!(
                        "systemd scope {unit} remained loaded after tmux pane cleanup: {}",
                        loaded.trim()
                    );
                }
                thread::sleep(Duration::from_millis(25));
            }
            scope_cleanup.0 = None;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn resumed_launch_persists_lease_metadata_across_respawn_and_removes_it_on_kill() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket = format!("atmux-test-lease-{}-{nonce}", std::process::id());
        let socket_base = env::var_os("TMUX_TMPDIR").map_or_else(env::temp_dir, PathBuf::from);
        let probe = DisposableTmux {
            socket_path: socket_base
                .join(format!("tmux-{}", rustix::process::geteuid().as_raw()))
                .join(&socket),
            socket,
            directory: env::temp_dir().join(format!(
                "atmux-test-lease-agent-{}-{nonce}",
                std::process::id()
            )),
        };
        fs::create_dir(&probe.directory).unwrap();
        let command = probe.directory.join("codex-lease-agent");
        fs::write(
            &command,
            "#!/bin/sh\nprintf 'lease-agent-ready\\n'\nsleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).unwrap();
        let profile = AgentProfile {
            name: "Lease fixture".to_owned(),
            harness: "codex".to_owned(),
            command: command.to_string_lossy().into_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };
        let candidate = ResumeCandidate::fixture(
            crate::old_sessions::ResumeHarness::Codex,
            "11111111-2222-4333-8444-555555555555",
        );
        let lease = format!("lease-v1-{}", "c".repeat(64));

        Tmux::with_socket_for_test(&probe.socket, || {
            Tmux::check()?;
            // Keep the isolated server alive after the resumed session is
            // killed so Drop can issue kill-server and tmux removes its socket.
            Tmux::output([
                "new-session",
                "-d",
                "-s",
                "lease-cleanup-canary",
                "/bin/sleep 2147483647",
            ])?;
            Tmux::launch_resumed(
                "lease-fixture",
                &probe.directory,
                &profile,
                None,
                &candidate,
                &lease,
                systemd_scope::prepare(&AgentResourcesConfig::default(), "lease-fixture")?,
            )?;
            let sessions = Tmux.sessions(&HashMap::new(), &StatusConfig::default())?;
            let resumed = sessions
                .iter()
                .find(|session| session.name == "lease-fixture")
                .context("resumed fixture was not discovered")?;
            assert_eq!(resumed.resume_lease.as_deref(), Some(lease.as_str()));
            assert!(Tmux::resume_lease_active(&lease)?);
            Tmux.kill("lease-fixture")?;
            assert!(!Tmux::resume_lease_active(&lease)?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    #[ignore = "requires a real isolated tmux server; run explicitly outside the parallel unit suite"]
    fn disposable_launcher_managed_claude_launch_receives_one_native_flag() {
        let probe = disposable_tmux("claude-resume");
        fs::create_dir(&probe.directory).unwrap();
        let command = probe.directory.join("claude-wrapper");
        let argv_file = probe.directory.join("argv");
        let interpolation_marker = probe.directory.join("interpolated");
        fs::write(
            &command,
            concat!(
                "#!/bin/sh\n",
                "while [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n",
                "[ \"$#\" -eq 0 ] || shift\n",
                "tmp_file=\"${ATMUX_TEST_ARGV}.tmp.$$\"\n",
                "printf '%s\\n' --dangerously-skip-permissions \"$@\" >\"$tmp_file\"\n",
                "/bin/mv \"$tmp_file\" \"$ATMUX_TEST_ARGV\"\n",
                "sleep 30\n",
            ),
        )
        .unwrap();
        fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).unwrap();
        let literal = format!("$(touch {})", interpolation_marker.display());
        let profile = AgentProfile {
            name: "Claude canary".to_owned(),
            harness: "claude".to_owned(),
            command: command.to_string_lossy().into_owned(),
            args: vec![literal.clone(), "--".to_owned()],
            env: BTreeMap::from([(
                "ATMUX_TEST_ARGV".to_owned(),
                argv_file.to_string_lossy().into_owned(),
            )]),
            inherit_discovered: false,
            claude_relaunch_permissions: Some(ClaudeRelaunchPermissions::LauncherProvides),
            modes: Vec::new(),
        };
        let session_id = "55555555-5555-4555-8555-555555555555";
        let candidate =
            ResumeCandidate::fixture(crate::old_sessions::ResumeHarness::Claude, session_id);
        let lease = format!("lease-v1-{}", "d".repeat(64));

        Tmux::with_socket_for_test(&probe.socket, || {
            Tmux::check()?;
            Tmux::launch_resumed(
                "claude-resume-canary",
                &probe.directory,
                &profile,
                None,
                &candidate,
                &lease,
                systemd_scope::prepare(&AgentResourcesConfig::default(), "claude-resume-canary")?,
            )?;
            let deadline = Instant::now() + Duration::from_secs(3);
            while !argv_file.exists() {
                if Instant::now() >= deadline {
                    bail!("disposable Claude wrapper did not record argv");
                }
                thread::sleep(Duration::from_millis(10));
            }
            let argv = fs::read_to_string(&argv_file)?;
            assert_eq!(
                argv.lines().collect::<Vec<_>>(),
                [CLAUDE_SKIP_PERMISSIONS_FLAG, "--resume", session_id,]
            );
            assert!(
                !interpolation_marker.exists(),
                "shell interpolation escaped argv quoting"
            );
            Tmux.kill("claude-resume-canary")?;
            assert!(!Tmux::resume_lease_active(&lease)?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn recognized_agent_pane_wins_over_active_shell_pane() {
        let source = concat!(
            "work\t0\t1\t123\t0\t1\t0\t1\t41\tbash\tsh\t/tmp\tshell\t%1\t\n",
            "work\t0\t1\t123\t0\t0\t1\t0\t42\tcodex\tenv codex\t/tmp\tagent\t%2\t\n",
        );
        let selected = select_session_panes(source, &ProcessTable::default());
        let (pane, agent) = selected.get("work").unwrap();

        assert_eq!(pane.pane_id, "%2");
        assert_eq!(*agent, AgentKind::Codex);
    }

    #[test]
    fn reserved_service_session_is_not_selected() {
        let source = concat!(
            "atmux-web\t0\t1\t123\t0\t1\t0\t1\t41\tbash\tsh\t/tmp\tservice\t%1\t\n",
            "work\t0\t1\t123\t0\t1\t0\t1\t42\tcodex\tcodex\t/tmp\twork\t%2\t\n",
        );

        let selected = select_session_panes(source, &ProcessTable::default());

        assert!(!selected.contains_key(RESERVED_SERVICE_SESSION));
        assert!(selected.contains_key("work"));
    }

    #[test]
    fn paste_preserves_multiline_text_and_uses_bracketed_paste() {
        assert_eq!(
            paste_buffer_args("atmux-buffer", "%7"),
            [
                "paste-buffer",
                "-d",
                "-p",
                "-r",
                "-b",
                "atmux-buffer",
                "-t",
                "%7",
            ]
        );
    }

    #[test]
    fn auto_compact_text_and_enter_are_one_fixed_tmux_command_list() {
        assert_eq!(
            auto_compact_delivery_args("%7"),
            [
                "send-keys",
                "-t",
                "%7",
                "-l",
                "/compact",
                ";",
                "send-keys",
                "-t",
                "%7",
                "Enter",
            ]
        );
    }

    #[test]
    fn interactive_special_keys_are_fixed_tmux_arguments() {
        for (action, expected) in [
            (PaneSpecialKey::Up, "Up"),
            (PaneSpecialKey::Down, "Down"),
            (PaneSpecialKey::Left, "Left"),
            (PaneSpecialKey::Right, "Right"),
            (PaneSpecialKey::Enter, "Enter"),
        ] {
            assert_eq!(
                special_key_args("%7; run-shell 'touch /tmp/pwned'", action),
                [
                    "send-keys",
                    "-t",
                    "%7; run-shell 'touch /tmp/pwned'",
                    expected,
                ],
            );
        }
    }

    #[test]
    fn cli_update_respawn_preserves_profile_model_effort_fast_and_exact_resume() {
        let profile = AgentProfile {
            name: "Sol".to_owned(),
            harness: "codex".to_owned(),
            command: "/owner/.local/bin/codex".to_owned(),
            args: vec!["--profile".to_owned(), "work".to_owned()],
            env: BTreeMap::from([("CODEX_HOME".to_owned(), "/owner/.codex-work".to_owned())]),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };
        let mode = ProfileMode {
            id: "sol-xhigh-fast".to_owned(),
            label: None,
            model: "gpt-5.6-sol".to_owned(),
            effort: Some("xhigh".to_owned()),
            service_tier: Some("fast".to_owned()),
        };
        let resume_arguments = crate::auto_update::resume_arguments(
            crate::auto_update::Harness::Codex,
            "11111111-1111-1111-1111-111111111111",
        )
        .unwrap();
        let invocation = build_native_relaunch_invocation(
            &profile,
            &mode,
            crate::auto_update::Harness::Codex,
            resume_arguments,
        )
        .unwrap();
        assert_eq!(
            invocation,
            [
                "env",
                "CODEX_HOME=/owner/.codex-work",
                "/owner/.local/bin/codex",
                "--profile",
                "work",
                "--model",
                "gpt-5.6-sol",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "-c",
                "service_tier=\"fast\"",
                "resume",
                "11111111-1111-1111-1111-111111111111",
            ]
        );
    }

    #[test]
    fn midnight_claude_update_respawn_keeps_keychain_profile_and_cwd() {
        let profile = AgentProfile {
            name: "max".to_owned(),
            harness: "claude".to_owned(),
            command: "claude".to_owned(),
            args: Vec::new(),
            env: BTreeMap::from([(
                "CLAUDE_CONFIG_DIR".to_owned(),
                "/Users/ryan/.claude-max".to_owned(),
            )]),
            inherit_discovered: false,
            claude_relaunch_permissions: Some(ClaudeRelaunchPermissions::LauncherProvides),
            modes: Vec::new(),
        };
        let mode = ProfileMode {
            id: "opus-high".to_owned(),
            label: None,
            model: "opus".to_owned(),
            effort: Some("high".to_owned()),
            service_tier: None,
        };
        let mut exact_profile = profile;
        exact_profile.command = "/Users/ryan/.local/share/claude/versions/2.2.0".to_owned();
        let resume_arguments = crate::auto_update::resume_arguments(
            crate::auto_update::Harness::Claude,
            "22222222-2222-2222-2222-222222222222",
        )
        .unwrap();
        let invocation = build_native_relaunch_invocation(
            &exact_profile,
            &mode,
            crate::auto_update::Harness::Claude,
            resume_arguments,
        )
        .unwrap();
        let command = shell_words::join(invocation);
        assert_eq!(
            respawn_pane_args("%9", "/Users/ryan/IdeaProjects/atmux", &command),
            [
                "respawn-pane",
                "-k",
                "-t",
                "%9",
                "-c",
                "/Users/ryan/IdeaProjects/atmux",
                command.as_str(),
            ]
        );
        assert!(command.contains("CLAUDE_CONFIG_DIR=/Users/ryan/.claude-max"));
        assert!(command.contains("/Users/ryan/.local/share/claude/versions/2.2.0"));
        assert!(command.contains("--model opus --effort high"));
        assert!(command.ends_with(
            "--dangerously-skip-permissions --permission-mode bypassPermissions --resume 22222222-2222-2222-2222-222222222222"
        ));
        assert!(!command.contains("/Users/ryan/.claude "));
        assert!(!command.contains("claude-max-wrapper"));
    }

    #[test]
    fn saved_claude_relaunch_normalizes_permission_flag_before_resume_and_double_dash() {
        let session_id = "33333333-3333-3333-3333-333333333333";
        let profile = AgentProfile {
            name: "Wrapped max".to_owned(),
            harness: "ClAuDe".to_owned(),
            command: "/owner/bin/claude-max-wrapper".to_owned(),
            args: vec![
                "--settings".to_owned(),
                "literal $(touch /tmp/never) ' quote".to_owned(),
                CLAUDE_PERMISSION_MODE_FLAG.to_owned(),
                "acceptEdits".to_owned(),
                "--permission-mode=auto".to_owned(),
                CLAUDE_SKIP_PERMISSIONS_FLAG.to_owned(),
                CLAUDE_SKIP_PERMISSIONS_FLAG.to_owned(),
                "--".to_owned(),
                CLAUDE_SKIP_PERMISSIONS_FLAG.to_owned(),
                "--resume".to_owned(),
                "literal-after-terminator".to_owned(),
            ],
            env: BTreeMap::from([(
                "CLAUDE_CONFIG_DIR".to_owned(),
                "/owner/.claude max".to_owned(),
            )]),
            inherit_discovered: false,
            claude_relaunch_permissions: Some(ClaudeRelaunchPermissions::AtmuxInjects),
            modes: Vec::new(),
        };
        let mode = ProfileMode {
            id: "opus-high".to_owned(),
            label: None,
            model: "opus".to_owned(),
            effort: Some("high".to_owned()),
            service_tier: None,
        };
        let candidate =
            ResumeCandidate::fixture(crate::old_sessions::ResumeHarness::Claude, session_id);

        let invocation =
            Tmux::build_launch_invocation(&profile, Some(&mode), Some(&candidate)).unwrap();

        assert_eq!(
            invocation,
            [
                "env",
                "CLAUDE_CONFIG_DIR=/owner/.claude max",
                "/owner/bin/claude-max-wrapper",
                "--settings",
                "literal $(touch /tmp/never) ' quote",
                CLAUDE_SKIP_PERMISSIONS_FLAG,
                CLAUDE_PERMISSION_MODE_FLAG,
                CLAUDE_BYPASS_PERMISSIONS_MODE,
                "--model",
                "opus",
                "--effort",
                "high",
                "--resume",
                session_id,
                "--",
                CLAUDE_SKIP_PERMISSIONS_FLAG,
                "--resume",
                "literal-after-terminator",
            ]
        );
        assert_eq!(
            active_arguments(&invocation[3..])
                .iter()
                .filter(|argument| argument.as_str() == CLAUDE_SKIP_PERMISSIONS_FLAG)
                .count(),
            1
        );
        let active = active_arguments(&invocation[3..]);
        assert_eq!(
            active
                .iter()
                .filter(|argument| argument.as_str() == CLAUDE_PERMISSION_MODE_FLAG)
                .count(),
            1
        );
        assert!(
            active
                .windows(2)
                .any(|pair| pair == [CLAUDE_PERMISSION_MODE_FLAG, CLAUDE_BYPASS_PERMISSIONS_MODE])
        );
        assert!(
            !active
                .iter()
                .any(|argument| argument == "acceptEdits" || argument == "--permission-mode=auto")
        );
        assert_eq!(
            shell_words::split(&shell_words::join(invocation.clone())).unwrap(),
            invocation
        );
    }

    #[test]
    fn launcher_provided_policy_preserves_opaque_order_and_adds_no_outer_flag() {
        let session_id = "66666666-6666-4666-8666-666666666666";
        let profile = AgentProfile {
            name: "Opaque wrapper".to_owned(),
            harness: "claude".to_owned(),
            command: "/owner/bin/claude-opaque".to_owned(),
            args: vec![
                "--wrapper-setting".to_owned(),
                "value".to_owned(),
                "--".to_owned(),
                "forwarded-literal".to_owned(),
            ],
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: Some(ClaudeRelaunchPermissions::LauncherProvides),
            modes: Vec::new(),
        };
        let mode = ProfileMode {
            id: "opus-high".to_owned(),
            label: None,
            model: "opus".to_owned(),
            effort: Some("high".to_owned()),
            service_tier: None,
        };
        let candidate =
            ResumeCandidate::fixture(crate::old_sessions::ResumeHarness::Claude, session_id);

        let invocation =
            Tmux::build_launch_invocation(&profile, Some(&mode), Some(&candidate)).unwrap();

        assert_eq!(
            invocation,
            [
                "env",
                "/owner/bin/claude-opaque",
                "--wrapper-setting",
                "value",
                "--",
                "forwarded-literal",
                "--model",
                "opus",
                "--effort",
                "high",
                "--resume",
                session_id,
            ]
        );
        assert!(
            !invocation
                .iter()
                .any(|argument| argument == CLAUDE_SKIP_PERMISSIONS_FLAG)
        );

        let mut ambiguous = profile;
        ambiguous.args.push("--continue".to_owned());
        assert!(
            Tmux::build_launch_invocation(&ambiguous, None, Some(&candidate)).is_err(),
            "a forwarded launcher resume selector must fail before adding another"
        );
    }

    #[test]
    fn fresh_claude_gets_bypass_permissions_while_saved_codex_does_not() {
        let fresh_claude = AgentProfile {
            name: "Fresh Claude".to_owned(),
            harness: "claude".to_owned(),
            command: "claude".to_owned(),
            args: vec![
                "--settings".to_owned(),
                "fresh.json".to_owned(),
                CLAUDE_SKIP_PERMISSIONS_FLAG.to_owned(),
                CLAUDE_PERMISSION_MODE_FLAG.to_owned(),
                "auto".to_owned(),
                CLAUDE_SKIP_PERMISSIONS_FLAG.to_owned(),
                "--".to_owned(),
                "literal".to_owned(),
            ],
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };
        let mode = ProfileMode {
            id: "opus-high".to_owned(),
            label: None,
            model: "opus".to_owned(),
            effort: Some("high".to_owned()),
            service_tier: None,
        };
        let fresh = Tmux::build_launch_invocation(&fresh_claude, Some(&mode), None).unwrap();
        assert_eq!(
            fresh,
            [
                "env",
                "claude",
                "--settings",
                "fresh.json",
                CLAUDE_SKIP_PERMISSIONS_FLAG,
                CLAUDE_PERMISSION_MODE_FLAG,
                CLAUDE_BYPASS_PERMISSIONS_MODE,
                "--model",
                "opus",
                "--effort",
                "high",
                "--",
                "literal",
            ]
        );

        let codex = AgentProfile {
            name: "Codex".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: vec![
                "--profile".to_owned(),
                "work".to_owned(),
                "--".to_owned(),
                "--resume".to_owned(),
            ],
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };
        let candidate = ResumeCandidate::fixture(
            crate::old_sessions::ResumeHarness::Codex,
            "44444444-4444-4444-4444-444444444444",
        );
        let resumed = Tmux::build_launch_invocation(&codex, None, Some(&candidate)).unwrap();
        assert!(
            !resumed
                .iter()
                .any(|argument| argument == CLAUDE_SKIP_PERMISSIONS_FLAG)
        );
        assert_eq!(
            resumed,
            [
                "env",
                "codex",
                "--profile",
                "work",
                "--",
                "--resume",
                "resume",
                "44444444-4444-4444-4444-444444444444",
            ]
        );
    }

    #[test]
    fn launch_label_looks_through_the_fixed_systemd_scope_wrapper() {
        let scope = systemd_scope::fixture_scope(
            "atmux-tmux-spawn-1-2-0123456789abcdef.scope",
            34_359_738_368,
        );
        let invocation = scope
            .wrap(vec![
                "env".to_owned(),
                "CODEX_HOME=/home/ryan/.codex-max".to_owned(),
                "/home/ryan/.local/bin/codex".to_owned(),
                "resume".to_owned(),
                "opaque-secret".to_owned(),
            ])
            .unwrap();
        let command = shell_words::join(invocation);

        assert_eq!(
            launch_command_label(&command),
            "CODEX_HOME=/home/ryan/.codex-max · /home/ryan/.local/bin/codex"
        );
        assert!(!launch_command_label(&command).contains("systemd-run"));
        assert!(!launch_command_label(&command).contains("opaque-secret"));
    }

    #[test]
    fn submit_sends_exactly_one_enter() {
        assert_eq!(submission_keys(true), ["Enter"]);
        assert!(submission_keys(false).is_empty());
    }

    #[test]
    fn standalone_submit_is_a_fixed_enter_command() {
        assert_eq!(["send-keys", "-t", "%7", "Enter"], submit_args("%7"));
    }

    #[test]
    fn special_prefix_action_sends_ctrl_b_twice() {
        assert_eq!(tmux_prefix_twice_keys(), ["C-b", "C-b"]);
    }

    #[test]
    fn model_observation_is_harness_specific_and_prefers_confirmed_changes() {
        let claude = "╭ Claude Code v2.1.226 ╮\n│ Sonnet 5 with xhigh effort │\n  ⎿  Set model to Haiku 4.5 for this session only\n";
        assert_eq!(
            observe_model(AgentKind::Claude, claude, Some("opus"), None),
            ModelObservation {
                version: Some("2.1.226".to_owned()),
                current: Some("haiku".to_owned()),
                effort: None,
                mode: None,
            }
        );
        let codex = "│ >_ OpenAI Codex (v0.147.0) │\n│ model: gpt-5.6-terra medium /model to change │\n• Model changed to gpt-5.6-luna medium\n";
        assert_eq!(
            observe_model(AgentKind::Codex, codex, None, None),
            ModelObservation {
                version: Some("0.147.0".to_owned()),
                current: Some("gpt-5.6-luna".to_owned()),
                effort: None,
                mode: None,
            }
        );
    }

    #[test]
    fn model_protocol_is_versioned_and_model_ids_are_data_only() {
        assert_eq!(known_models(AgentKind::Claude, "2.1.224").len(), 5);
        assert_eq!(known_models(AgentKind::Codex, "0.147.0").len(), 7);
        assert!(known_models(AgentKind::Claude, "2.2.0").is_empty());
        assert!(known_models(AgentKind::Codex, "0.148.0").is_empty());
        for model in [
            "default",
            "claude-opus-5",
            "gpt-5.6-sol",
            "provider:model_1",
        ] {
            assert!(valid_model_id(model));
        }
        for model in ["", "gpt 5", "gpt;touch-pwned", "$(id)", "/model"] {
            assert!(!valid_model_id(model));
        }
    }

    #[test]
    fn claude_resume_command_uses_only_fixed_server_side_arguments() {
        let session_id = "11111111-1111-1111-1111-111111111111";
        let command = claude_resume_command(
            Path::new("/tmp/Claude Code/claude"),
            Path::new("/tmp/.claude max"),
            session_id,
        )
        .unwrap();
        assert_eq!(
            shell_words::split(&command).unwrap(),
            vec![
                "env",
                "CLAUDE_CONFIG_DIR=/tmp/.claude max",
                "/tmp/Claude Code/claude",
                CLAUDE_SKIP_PERMISSIONS_FLAG,
                CLAUDE_PERMISSION_MODE_FLAG,
                CLAUDE_BYPASS_PERMISSIONS_MODE,
                "--resume",
                session_id,
            ]
        );
        let invocation = claude_resume_invocation(
            Path::new("/tmp/Claude Code/claude"),
            Path::new("/tmp/.claude max"),
            session_id,
        )
        .unwrap();
        assert_eq!(
            invocation
                .iter()
                .filter(|argument| argument.as_str() == CLAUDE_SKIP_PERMISSIONS_FLAG)
                .count(),
            1
        );
        assert!(
            invocation
                .windows(2)
                .any(|pair| pair == [CLAUDE_PERMISSION_MODE_FLAG, CLAUDE_BYPASS_PERMISSIONS_MODE])
        );
        assert!(
            invocation
                .windows(2)
                .any(|pair| pair == ["--resume", session_id])
        );
        assert!(
            claude_resume_command(
                Path::new("/tmp/claude"),
                Path::new("/tmp/.claude"),
                "not-a-session"
            )
            .is_err()
        );
        assert!(
            claude_resume_command(Path::new("claude"), Path::new("/tmp/.claude"), session_id)
                .is_err()
        );
    }

    #[test]
    fn claude_resume_recheck_fails_typed_before_tmux_respawn() {
        let missing = env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap()
            .join(".local/share/claude/versions/atmux-definitely-missing");
        let error = Tmux::resume_claude(
            "%4294967295",
            Path::new("/tmp"),
            &missing,
            Path::new("/tmp/.claude"),
            "11111111-1111-1111-1111-111111111111",
            systemd_scope::prepare(&AgentResourcesConfig::default(), "missing-claude").unwrap(),
        )
        .unwrap_err();
        assert!(
            error
                .chain()
                .any(<dyn std::error::Error>::is::<ClaudeResumeUnavailable>)
        );
        assert!(!error.to_string().contains("tmux respawn-pane"));
    }

    #[test]
    fn native_model_menus_must_match_every_expected_row() {
        let claude = concat!(
            "Select model\n",
            "  1. Default (recommended)\n",
            "  2. Opus (1M context)\n",
            "  3. Fable\n",
            "❯ 4. Sonnet ✔\n",
            "  5. Haiku\n",
        );
        assert_eq!(
            menu_selection(claude, "Select model", CLAUDE_MODELS),
            Some(3)
        );
        assert_eq!(
            menu_selection(
                &claude.replace("5. Haiku", "5. Mystery"),
                "Select model",
                CLAUDE_MODELS
            ),
            None
        );

        let codex = concat!(
            "Select Model and Effort\n",
            "  1. gpt-5.6-sol (default)\n",
            "› 2. gpt-5.6-terra (current)\n",
            "  3. gpt-5.6-luna\n",
            "  4. gpt-5.5\n",
            "  5. gpt-5.4\n",
            "  6. gpt-5.4-mini\n",
            "  7. gpt-5.3-codex-spark\n",
        );
        assert_eq!(
            menu_selection(codex, "Select Model and Effort", CODEX_MODELS),
            Some(1)
        );
    }

    #[test]
    fn confirmations_and_reasoning_menu_are_target_specific() {
        assert!(claude_confirmation(
            "⎿ Set model to Fable 5 for this session only",
            "fable"
        ));
        assert!(!claude_confirmation(
            "⎿ Set model to Fable 5 and saved as your default",
            "fable"
        ));
        assert!(codex_confirmation(
            "• Model changed to gpt-5.6-sol xhigh",
            "gpt-5.6-sol"
        ));
        let effort = concat!(
            "Select Reasoning Level for gpt-5.6-sol\n",
            "  1. Low\n",
            "› 2. Medium (default)\n",
            "  3. High\n",
            "  4. Extra high\n",
            "  5. More reasoning…\n",
        );
        assert_eq!(
            reason_menu_selection(effort, Some("xhigh")),
            Some(ReasonMenu::Move {
                current: 1,
                target: 3,
            })
        );
    }

    #[test]
    fn tmux_socket_override_is_narrowly_validated() {
        assert!(valid_socket_name("atmux-model-123"));
        assert!(!valid_socket_name("../default"));
        assert!(!valid_socket_name("name with spaces"));
        assert!(valid_tmux_session_id("$123"));
        assert!(!valid_tmux_session_id("agent"));
        assert!(!valid_tmux_session_id("$"));
        assert_eq!(
            parse_tmux_created_target("$123\t%456"),
            Some(("$123".to_owned(), "%456".to_owned()))
        );
        assert!(parse_tmux_created_target("agent\t%456").is_none());
    }

    #[test]
    fn recognizes_missing_tmux_server_diagnostics() {
        assert!(is_missing_server_message(
            "no server running on /tmp/tmux-1000/default"
        ));
        assert!(is_missing_server_message(
            "error connecting to /tmp/tmux-1000/default (No such file or directory)"
        ));
        assert!(!is_missing_server_message(
            "error connecting to /tmp/tmux-1000/default (Permission denied)"
        ));
        assert!(!is_missing_server_message(
            "failed to connect to server: Permission denied"
        ));
    }

    #[test]
    fn validates_agent_commands() {
        assert!(command_available("sh"));
        assert!(command_available("/bin/sh"));
        assert!(!command_available("/definitely/missing/atmux-agent"));
    }

    #[test]
    fn launch_label_keeps_the_wrapper_and_safe_claude_config_without_arguments() {
        assert_eq!(
            launch_command_label(
                "env CLAUDE_CONFIG_DIR=/home/ryan/.claude-max API_TOKEN=secret /home/ryan/.local/bin/claude --resume sensitive-id"
            ),
            "CLAUDE_CONFIG_DIR=/home/ryan/.claude-max · /home/ryan/.local/bin/claude"
        );
        assert_eq!(
            launch_command_label("env CLAUDE_CONFIG_DIR=~/.claude-max claude"),
            "CLAUDE_CONFIG_DIR=~/.claude-max (unexpanded) · claude"
        );
        assert_eq!(
            launch_command_label("env CODEX_HOME=/home/ryan/.codex-work codex --profile work"),
            "CODEX_HOME=/home/ryan/.codex-work · codex"
        );
        assert_eq!(
            launch_command_label("\"cd ~/work && exec claude-max --resume sensitive-id\""),
            "claude-max"
        );
    }

    #[test]
    fn profile_label_prefers_persisted_metadata_and_safely_infers_existing_sessions() {
        assert_eq!(
            agent_profile_label(
                "env CLAUDE_CONFIG_DIR=/home/ryan/.claude-max claude",
                "",
                AgentKind::Claude
            ),
            "claude-max"
        );
        assert_eq!(
            agent_profile_label(
                "env CODEX_HOME=/home/ryan/.codex codex",
                "",
                AgentKind::Codex
            ),
            "Default"
        );
        assert_eq!(
            agent_profile_label("claude-hd --resume secret", "", AgentKind::Claude),
            "claude-hd"
        );
        assert_eq!(
            agent_profile_label("claude", "Max", AgentKind::Claude),
            "Max"
        );
        assert_eq!(
            agent_profile_label("claude", "../../secret", AgentKind::Claude),
            "Default"
        );
    }

    #[test]
    fn process_elapsed_and_agent_identity_are_narrowly_parsed() {
        assert_eq!(parse_elapsed("02:03"), Some(Duration::from_secs(123)));
        assert_eq!(
            parse_elapsed("1-02:03:04"),
            Some(Duration::from_secs(93_784))
        );
        assert!(command_is_agent_process(
            "node /Users/ryan/bin/codex",
            AgentKind::Codex
        ));
        assert!(command_is_agent_process(
            "/Users/ryan/.local/bin/claude --model fable",
            AgentKind::Claude
        ));
        assert!(!command_is_agent_process(
            "sh -c env codex",
            AgentKind::Codex
        ));
        assert!(!command_is_agent_process(
            "bash -lc claude-max",
            AgentKind::Claude
        ));
    }

    #[test]
    fn native_claude_executable_requires_a_secure_current_user_install() {
        let fixture = NativeClaudeFixture::new("identity", "2.1.241");
        let uid = rustix::process::geteuid().as_raw();
        let valid = fixture.executable.to_string_lossy();
        assert!(native_claude_version_executable_in(
            &valid,
            &fixture.home,
            uid
        ));

        assert!(!native_claude_version_executable_in(
            &fixture.versions.join("9.9.9").to_string_lossy(),
            &fixture.home,
            uid
        ));
        assert!(!native_claude_version_executable_in(
            &valid,
            &fixture.home,
            uid.wrapping_add(1)
        ));
        assert!(!native_claude_version_executable_in(
            &fixture
                .home
                .join(".local/share/claude/versions/../versions/2.1.241")
                .to_string_lossy(),
            &fixture.home,
            uid
        ));

        let non_executable = fixture.versions.join("2.1.242");
        fs::write(&non_executable, "not executable").unwrap();
        fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!native_claude_version_executable_in(
            &non_executable.to_string_lossy(),
            &fixture.home,
            uid
        ));

        let directory = fixture.versions.join("2.1.243");
        fs::create_dir(&directory).unwrap();
        assert!(!native_claude_version_executable_in(
            &directory.to_string_lossy(),
            &fixture.home,
            uid
        ));

        let symlink_target = fixture.root.join("outside-native-binary");
        fs::write(&symlink_target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&symlink_target, fs::Permissions::from_mode(0o700)).unwrap();
        let linked_file = fixture.versions.join("2.1.244");
        symlink(&symlink_target, &linked_file).unwrap();
        assert!(!native_claude_version_executable_in(
            &linked_file.to_string_lossy(),
            &fixture.home,
            uid
        ));

        let arbitrary = fixture.home.join("bin/2.1.245");
        fs::create_dir_all(arbitrary.parent().unwrap()).unwrap();
        fs::write(&arbitrary, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&arbitrary, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!native_claude_version_executable_in(
            &arbitrary.to_string_lossy(),
            &fixture.home,
            uid
        ));

        let malformed = fixture.versions.join("current");
        fs::write(&malformed, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&malformed, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!native_claude_version_executable_in(
            &malformed.to_string_lossy(),
            &fixture.home,
            uid
        ));

        let linked_home = fixture.root.join("linked-home");
        let linked_share = linked_home.join(".local/share");
        let real_claude = fixture.root.join("real-claude");
        let real_versions = real_claude.join("versions");
        fs::create_dir_all(&linked_share).unwrap();
        fs::create_dir_all(&real_versions).unwrap();
        let through_link = real_versions.join("2.1.246");
        fs::write(&through_link, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&through_link, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&real_claude, linked_share.join("claude")).unwrap();
        assert!(!native_claude_version_executable_in(
            &linked_home
                .join(".local/share/claude/versions/2.1.246")
                .to_string_lossy(),
            &linked_home,
            uid
        ));

        let linked_home_path = fixture.root.join("home-link");
        symlink(&fixture.home, &linked_home_path).unwrap();
        assert!(!native_claude_version_executable_in(
            &linked_home_path
                .join(".local/share/claude/versions/2.1.241")
                .to_string_lossy(),
            &linked_home_path,
            uid
        ));
    }

    #[test]
    fn native_codex_process_wins_over_its_node_launcher() {
        let table = ProcessTable {
            entries: HashMap::from([
                (10, (1, Some(Duration::from_secs(20)), "sh".to_owned())),
                (
                    11,
                    (
                        10,
                        Some(Duration::from_secs(19)),
                        "node /opt/codex/bin/codex".to_owned(),
                    ),
                ),
                (
                    12,
                    (
                        11,
                        Some(Duration::from_secs(18)),
                        "/opt/codex/vendor/bin/codex".to_owned(),
                    ),
                ),
            ]),
        };
        assert_eq!(
            table
                .agent_process_under(10, AgentKind::Codex)
                .map(|value| value.0),
            Some(12)
        );
    }

    #[test]
    fn native_versioned_claude_process_wins_below_a_resume_wrapper() {
        let fixture = NativeClaudeFixture::new("resume-wrapper", "2.1.241");
        let uid = rustix::process::geteuid().as_raw();
        let table = ProcessTable {
            entries: HashMap::from([
                (20, (1, Some(Duration::from_secs(20)), "sh".to_owned())),
                (
                    21,
                    (
                        20,
                        Some(Duration::from_secs(19)),
                        "claude --resume 11111111-1111-1111-1111-111111111111".to_owned(),
                    ),
                ),
                (
                    22,
                    (
                        21,
                        Some(Duration::from_secs(18)),
                        format!(
                            "{} --resume 11111111-1111-1111-1111-111111111111",
                            fixture.executable.display()
                        ),
                    ),
                ),
            ]),
        };
        assert_eq!(
            table
                .agent_process_under_ranked(20, AgentKind::Claude, |command, kind| {
                    agent_process_rank_in(command, kind, Some(&fixture.home), uid)
                })
                .map(|value| value.0),
            Some(22)
        );
    }

    #[test]
    fn popup_command_reuses_the_current_tmux_socket() {
        let command = popup_attach_command("review one", "/tmp/tmux-1000/custom,42,0").unwrap();
        let tmux = tmux_program().to_string_lossy().into_owned();
        assert_eq!(
            shell_words::split(&command).unwrap(),
            vec![
                "env".to_owned(),
                "-u".to_owned(),
                "TMUX".to_owned(),
                "-u".to_owned(),
                "TMUX_PANE".to_owned(),
                tmux,
                "-S".to_owned(),
                "/tmp/tmux-1000/custom".to_owned(),
                "attach-session".to_owned(),
                "-t".to_owned(),
                "review one".to_owned(),
            ]
        );
    }
}
