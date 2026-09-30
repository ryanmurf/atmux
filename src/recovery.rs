//! Host-scoped restart recovery ("Quick Resume").
//!
//! This deliberately exposes one fixed operation per machine rather than a
//! generic command runner.  Only the node's configured, locally owned roster
//! script is eligible, browser callers cannot supply a path or arguments,
//! output is never returned, and one process may run at a time.  The script
//! must carry the canonical transactional helper block and must launch every
//! roster entry through the bridge the node's own memory policy requires.

use std::{
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use rustix::{
    fs::{Mode, OFlags},
    process::{Pid, Signal, geteuid, kill_process_group, test_kill_process_group},
};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command, sync::Mutex};

use crate::config::Config;

const SCRIPT_MARKER: &str = "ATMUX_QUICK_RESUME_IDEMPOTENT_V1";
const SCOPED_EXEC_MARKER: &str = "ATMUX_QUICK_RESUME_SCOPED_EXEC_V1";
const DIRECT_EXEC_MARKER: &str = "ATMUX_QUICK_RESUME_DIRECT_EXEC_V1";
const TRANSACTION_BEGIN: &[u8] = b"\n# ATMUX_QUICK_RESUME_TRANSACTION_BEGIN\n";
const TRANSACTION_END: &[u8] = b"# ATMUX_QUICK_RESUME_TRANSACTION_END\n";
/// Tron's canonical helper block: the transactional helpers with the
/// memory-scoped launch bridge exactly as the checked-in template ships it.
/// Every other machine's block is this text with its bridge substituted.
const CANONICAL_SCOPED_HELPERS: &str =
    include_str!("../tests/fixtures/resume_tron_transactional_helpers.sh");
const SCOPED_SEND_BLOCK: &str =
    include_str!("../deploy/systemd/resume-tron-scoped-exec-block.bash");
const DIRECT_SEND_BLOCK: &str = include_str!("../deploy/quick-resume/send-direct.bash");
const SCOPED_COMMAND_PREFIX: &str = "  local scoped_exec_command='";
const SCOPED_COMMAND_SUFFIX: &str = "'";
const SERVICE_OVERRIDE_IF_LINE: &str = "  if [ \"$unit_session\" = atmux-web ]; then";
const SERVICE_OVERRIDE_CAP_PREFIX: &str =
    "    scoped_exec_command+=' --recovery-service-memory-max-bytes ";
const SERVICE_OVERRIDE_CAP_SUFFIX: &str = "'";
const SERVICE_OVERRIDE_END_LINE: &str = "  fi";
const MAX_SCRIPT_BYTES: u64 = 1024 * 1024;
const RUN_TIMEOUT: Duration = Duration::from_secs(180);
#[cfg(not(test))]
const PROCESS_GROUP_GRACE: Duration = Duration::from_secs(5);
#[cfg(test)]
const PROCESS_GROUP_GRACE: Duration = Duration::from_millis(100);
// Keep the interpreter absolute and platform-pinned: the recovery child runs
// with an empty environment, so PATH lookup is intentionally unavailable.
// Linux installs bash in /usr/bin, while macOS ships it in /bin.
#[cfg(target_os = "macos")]
const BASH_COMMAND: &str = "/bin/bash";
#[cfg(not(target_os = "macos"))]
const BASH_COMMAND: &str = "/usr/bin/bash";
const DEFAULT_SCRIPT_NAME: &str = "quick-resume.sh";
const LOCK_FILE_NAME: &str = "quick-resume.lock";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    Unavailable,
    #[default]
    Idle,
    Running,
    Succeeded,
    Failed,
    TimedOut,
}

/// Safe recovery state suitable for a browser or federated peer.
///
/// It intentionally contains neither subprocess output nor local paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryStatus {
    pub machine: String,
    pub available: bool,
    pub phase: RecoveryPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    pub message: String,
}

#[derive(Debug)]
pub enum RecoveryStartError {
    Unavailable(String),
    Running,
}

impl std::fmt::Display for RecoveryStartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => formatter.write_str(message),
            Self::Running => formatter.write_str("Quick Resume is already running"),
        }
    }
}

impl std::error::Error for RecoveryStartError {}

/// How every roster entry must be launched on this node.
///
/// A node with a configured per-agent memory cap accepts only the scoped
/// bridge, so recovery can never start an unbounded worker there.  A node
/// without that policy accepts only the direct bridge, because `scoped-exec`
/// would fail closed on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LaunchBridge {
    Scoped {
        /// The configuration the daemon itself runs with.  The bridge must
        /// name the same file so the roster enters the policy in effect.
        config_path: Option<PathBuf>,
    },
    Direct,
}

impl LaunchBridge {
    const fn marker(&self) -> &'static str {
        match self {
            Self::Scoped { .. } => SCOPED_EXEC_MARKER,
            Self::Direct => DIRECT_EXEC_MARKER,
        }
    }
}

/// The fixed environment a roster script runs with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostEnvironment {
    home: PathBuf,
    user: String,
    path: String,
}

impl HostEnvironment {
    fn capture() -> Option<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))?;
        let user = ["USER", "LOGNAME"]
            .iter()
            .find_map(|key| std::env::var(key).ok())
            .filter(|user| !user.is_empty())
            .or_else(|| {
                home.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })?;
        let path = recovery_path(&home);
        Some(Self { home, user, path })
    }

    #[cfg(test)]
    fn fixture(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            user: "fixture".to_owned(),
            path: recovery_path(home),
        }
    }
}

/// The sanitized PATH a roster runs with: the user's launcher directories
/// plus the system directories, on every supported platform.
fn recovery_path(home: &Path) -> String {
    let mut entries = vec![
        "/usr/local/sbin".to_owned(),
        "/usr/local/bin".to_owned(),
        "/usr/sbin".to_owned(),
        "/usr/bin".to_owned(),
        "/sbin".to_owned(),
        "/bin".to_owned(),
    ];
    if cfg!(target_os = "macos") {
        entries.insert(0, "/opt/homebrew/bin".to_owned());
    } else {
        entries.push("/snap/bin".to_owned());
    }
    entries.push(home.join(".asdf/shims").to_string_lossy().into_owned());
    entries.push(home.join(".local/bin").to_string_lossy().into_owned());
    entries.join(":")
}

/// Where the single-flight lock lives: the user's private runtime directory.
fn runtime_directory(uid: u32) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute() && dir.is_dir())
    {
        return Some(dir.join("atmux"));
    }
    let linux = PathBuf::from(format!("/run/user/{uid}"));
    if linux.is_dir() {
        return Some(linux.join("atmux"));
    }
    // macOS has no /run/user; launchd gives each user a private, 0700
    // temporary directory instead.
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute() && dir.is_dir())
        .map(|dir| dir.join("atmux"))
}

#[derive(Debug)]
struct RecoveryInner {
    script: PathBuf,
    bridge: LaunchBridge,
    environment: Option<HostEnvironment>,
    runtime_dir: Option<PathBuf>,
    required_commands: Vec<PathBuf>,
    timeout: Duration,
    state: Mutex<RecoveryStatus>,
}

#[derive(Debug)]
struct ValidatedScript {
    contents: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScriptProblem {
    Missing,
    Invalid,
}

#[derive(Debug)]
enum LockError {
    Busy,
    Unsafe,
}

/// Cloneable single-flight handle for one owning node.
#[derive(Clone, Debug)]
pub struct RecoveryRunner {
    inner: Arc<RecoveryInner>,
}

impl RecoveryRunner {
    /// Builds this node's runner from its configuration.
    ///
    /// The roster script is `[recovery].script`, or `quick-resume.sh` beside
    /// the configuration file.  The launch bridge follows the node's memory
    /// policy, and the environment comes from the daemon's own identity.
    #[must_use]
    pub fn production(config: &Config) -> Self {
        let uid = geteuid().as_raw();
        let config_path = config.source_path.clone().or_else(|| Config::path().ok());
        let script = config.recovery.script.clone().unwrap_or_else(|| {
            config_path.as_deref().and_then(Path::parent).map_or_else(
                || PathBuf::from(DEFAULT_SCRIPT_NAME),
                |dir| dir.join(DEFAULT_SCRIPT_NAME),
            )
        });
        let bridge = if config.agent_resources.memory_max_bytes.is_some() {
            LaunchBridge::Scoped { config_path }
        } else {
            LaunchBridge::Direct
        };
        Self::new(
            &config.node.id,
            script,
            bridge,
            HostEnvironment::capture(),
            runtime_directory(uid),
            RUN_TIMEOUT,
            config.recovery.required_commands.clone(),
        )
    }

    fn new(
        machine: &str,
        script: PathBuf,
        bridge: LaunchBridge,
        environment: Option<HostEnvironment>,
        runtime_dir: Option<PathBuf>,
        timeout: Duration,
        required_commands: Vec<PathBuf>,
    ) -> Self {
        let mut inner = RecoveryInner {
            script,
            bridge,
            environment,
            runtime_dir,
            required_commands,
            timeout,
            state: Mutex::new(RecoveryStatus {
                machine: machine.to_owned(),
                available: false,
                phase: RecoveryPhase::Unavailable,
                started_at_ms: None,
                finished_at_ms: None,
                message: String::new(),
            }),
        };
        let (available, message) = match inner.probe() {
            Ok(_) => (true, READY_MESSAGE.to_owned()),
            Err(message) => (false, message),
        };
        {
            let state = inner.state.get_mut();
            state.available = available;
            state.phase = if available {
                RecoveryPhase::Idle
            } else {
                RecoveryPhase::Unavailable
            };
            state.message = message;
        }
        Self {
            inner: Arc::new(inner),
        }
    }

    /// A runner for control-plane tests: no roster exists, so every start
    /// fails closed without touching the filesystem or spawning anything.
    #[cfg(test)]
    pub(crate) fn detached(machine: &str) -> Self {
        Self::new(
            machine,
            PathBuf::from("/nonexistent/atmux-recovery/quick-resume.sh"),
            LaunchBridge::Direct,
            None,
            None,
            RUN_TIMEOUT,
            Vec::new(),
        )
    }

    pub async fn status(&self) -> RecoveryStatus {
        let mut state = self.inner.state.lock().await;
        if state.phase != RecoveryPhase::Running {
            match self.inner.probe() {
                Ok(_) => {
                    state.available = true;
                    if state.phase == RecoveryPhase::Unavailable {
                        state.phase = RecoveryPhase::Idle;
                        READY_MESSAGE.clone_into(&mut state.message);
                    }
                }
                Err(message) => {
                    state.available = false;
                    state.phase = RecoveryPhase::Unavailable;
                    state.message = message;
                }
            }
        }
        state.clone()
    }

    /// Starts the fixed recovery script and returns immediately with `running`.
    ///
    /// The returned task state is safe to poll; stdout and stderr are discarded
    /// so a future edit to the operator script cannot leak credentials through
    /// the API or consume unbounded server memory.
    ///
    /// # Errors
    ///
    /// Returns [`RecoveryStartError::Running`] while this process has a run in
    /// flight, or [`RecoveryStartError::Unavailable`] when the fixed script
    /// fails validation or recovery is not set up on this machine.
    pub async fn start(&self) -> Result<RecoveryStatus, RecoveryStartError> {
        let mut state = self.inner.state.lock().await;
        if state.phase == RecoveryPhase::Running {
            return Err(RecoveryStartError::Running);
        }
        let script = match self.inner.probe() {
            Ok(script) => script,
            Err(message) => {
                state.available = false;
                state.phase = RecoveryPhase::Unavailable;
                state.message = message;
                return Err(RecoveryStartError::Unavailable(state.message.clone()));
            }
        };
        let Some(environment) = self.inner.environment.clone() else {
            state.available = false;
            state.phase = RecoveryPhase::Unavailable;
            NO_IDENTITY_MESSAGE.clone_into(&mut state.message);
            return Err(RecoveryStartError::Unavailable(state.message.clone()));
        };
        let lock = match self
            .inner
            .runtime_dir
            .as_deref()
            .ok_or(LockError::Unsafe)
            .and_then(acquire_runtime_lock)
        {
            Ok(lock) => lock,
            Err(LockError::Busy) => return Err(RecoveryStartError::Running),
            Err(LockError::Unsafe) => {
                state.available = false;
                state.phase = RecoveryPhase::Unavailable;
                LOCK_MESSAGE.clone_into(&mut state.message);
                return Err(RecoveryStartError::Unavailable(state.message.clone()));
            }
        };

        let started_at_ms = now_ms();
        state.available = true;
        state.phase = RecoveryPhase::Running;
        state.started_at_ms = Some(started_at_ms);
        state.finished_at_ms = None;
        "Restoring missing sessions; existing sessions are preserved"
            .clone_into(&mut state.message);
        let started = state.clone();
        drop(state);

        let runner = self.clone();
        tokio::spawn(async move {
            let outcome = run_script(script, &environment, lock, runner.inner.timeout).await;
            let mut state = runner.inner.state.lock().await;
            state.finished_at_ms = Some(now_ms());
            match outcome {
                ScriptOutcome::Succeeded => {
                    state.phase = RecoveryPhase::Succeeded;
                    "Recovery script finished; sessions will appear as they become ready"
                        .clone_into(&mut state.message);
                }
                ScriptOutcome::Failed(code) => {
                    state.phase = RecoveryPhase::Failed;
                    state.message = code.map_or_else(
                        || "Recovery script was terminated".to_owned(),
                        |code| format!("Recovery script exited with status {code}"),
                    );
                }
                ScriptOutcome::TimedOut => {
                    state.phase = RecoveryPhase::TimedOut;
                    "Recovery stopped after its three-minute safety limit"
                        .clone_into(&mut state.message);
                }
            }
        });
        Ok(started)
    }

    #[cfg(test)]
    fn fixture(machine: &str, script: &Path, timeout: Duration) -> Self {
        Self::fixture_with_bridge(machine, script, timeout, LaunchBridge::Direct)
    }

    #[cfg(test)]
    fn fixture_with_bridge(
        machine: &str,
        script: &Path,
        timeout: Duration,
        bridge: LaunchBridge,
    ) -> Self {
        let directory = script.parent().unwrap();
        Self::new(
            machine,
            script.to_path_buf(),
            bridge,
            Some(HostEnvironment::fixture(directory)),
            Some(directory.join("runtime")),
            timeout,
            Vec::new(),
        )
    }
}

const READY_MESSAGE: &str = "Ready to restore this machine's saved session roster";
const NO_IDENTITY_MESSAGE: &str = "Quick Resume cannot determine this machine's user identity";
const LOCK_MESSAGE: &str = "Quick Resume's secure runtime lock directory is unavailable";

impl RecoveryInner {
    /// Re-validates everything a run depends on and returns the script bytes
    /// to execute, or the user-facing reason recovery is unavailable.
    fn probe(&self) -> Result<ValidatedScript, String> {
        let script =
            validate_script(&self.script, &self.bridge).map_err(|problem| match problem {
                ScriptProblem::Missing => {
                    "Quick Resume roster script is not installed on this machine".to_owned()
                }
                ScriptProblem::Invalid => {
                    "Quick Resume roster script fails its safety checks".to_owned()
                }
            })?;
        let Some(environment) = &self.environment else {
            return Err(NO_IDENTITY_MESSAGE.to_owned());
        };
        if !required_commands_available(&self.required_commands, &environment.path) {
            return Err(
                "Quick Resume roster commands are missing or not executable on this machine"
                    .to_owned(),
            );
        }
        match &self.runtime_dir {
            Some(dir) if validate_runtime_location(dir).is_ok() => {}
            _ => return Err(LOCK_MESSAGE.to_owned()),
        }
        Ok(script)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScriptOutcome {
    Succeeded,
    Failed(Option<i32>),
    TimedOut,
}

async fn run_script(
    script: ValidatedScript,
    environment: &HostEnvironment,
    _lock: File,
    timeout: Duration,
) -> ScriptOutcome {
    // Execute the already-opened, validated bytes instead of reopening a path
    // after validation. The child leads a new process group so timeout cleanup
    // reaches every descendant the recovery script started.
    let mut command = Command::new(BASH_COMMAND);
    configure_script_environment(&mut command, environment);
    command
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let Ok(mut child) = command.spawn() else {
        return ScriptOutcome::Failed(None);
    };
    let Some(group) = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
    else {
        let _ = child.kill().await;
        return ScriptOutcome::Failed(None);
    };
    let Some(mut stdin) = child.stdin.take() else {
        terminate_process_group(&mut child, group).await;
        return ScriptOutcome::Failed(None);
    };
    let deadline = Instant::now() + timeout;
    let wrote_script = tokio::time::timeout(timeout, async {
        stdin.write_all(&script.contents).await?;
        stdin.shutdown().await
    })
    .await;
    if !matches!(wrote_script, Ok(Ok(()))) {
        terminate_process_group(&mut child, group).await;
        return ScriptOutcome::TimedOut;
    }
    drop(stdin);
    match tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        child.wait(),
    )
    .await
    {
        Ok(Ok(status)) if status.success() => ScriptOutcome::Succeeded,
        Ok(Ok(status)) => ScriptOutcome::Failed(status.code()),
        Ok(Err(_)) => ScriptOutcome::Failed(None),
        Err(_) => {
            terminate_process_group(&mut child, group).await;
            ScriptOutcome::TimedOut
        }
    }
}

fn configure_script_environment(command: &mut Command, environment: &HostEnvironment) {
    // Bash evaluates BASH_ENV before stdin and imports exported shell
    // functions. Start from an empty environment so the pinned script bytes
    // are the only shell program that can execute, then add only the fixed
    // identity data a roster and its verification commands require.
    command.env_clear().envs([
        ("HOME", environment.home.as_os_str().to_owned()),
        ("LANG", "C.UTF-8".into()),
        ("LOGNAME", environment.user.clone().into()),
        ("PATH", environment.path.clone().into()),
        ("USER", environment.user.clone().into()),
    ]);
}

/// Every configured roster command plus the interpreter and a `tmux` on the
/// sanitized PATH must exist and be executable before recovery is offered.
fn required_commands_available(commands: &[PathBuf], path: &str) -> bool {
    executable_file(Path::new(BASH_COMMAND))
        && resolve_on_path("tmux", path).is_some()
        && commands
            .iter()
            .all(|command| command.is_absolute() && executable_file(command))
}

fn executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
}

fn resolve_on_path(name: &str, path: &str) -> Option<PathBuf> {
    path.split(':')
        .filter(|entry| !entry.is_empty())
        .map(|entry| Path::new(entry).join(name))
        .find(|candidate| executable_file(candidate))
}

async fn terminate_process_group(child: &mut tokio::process::Child, group: Pid) {
    let _ = kill_process_group(group, Signal::TERM);
    let reaped = tokio::time::timeout(PROCESS_GROUP_GRACE, child.wait()).await;
    // The leader can exit before a TERM-ignoring descendant. Probe the group
    // itself before declaring cleanup complete, then reap the leader if the
    // grace-period wait did not already do so.
    if test_kill_process_group(group).is_ok() {
        let _ = kill_process_group(group, Signal::KILL);
    }
    if reaped.is_err() {
        let _ = child.wait().await;
    }
}

fn validate_script(path: &Path, bridge: &LaunchBridge) -> Result<ValidatedScript, ScriptProblem> {
    let euid = geteuid().as_raw();
    if matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return Err(ScriptProblem::Missing);
    }
    validate_secure_ancestry(path, euid).map_err(|()| ScriptProblem::Invalid)?;
    let path_metadata = fs::symlink_metadata(path).map_err(|_| ScriptProblem::Invalid)?;
    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| ScriptProblem::Invalid)?;
    let mut file = File::from(descriptor);
    let metadata = file.metadata().map_err(|_| ScriptProblem::Invalid)?;
    if !metadata.file_type().is_file()
        || metadata.len() > MAX_SCRIPT_BYTES
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
        || metadata.uid() != euid
        || metadata.nlink() != 1
        || path_metadata.dev() != metadata.dev()
        || path_metadata.ino() != metadata.ino()
    {
        return Err(ScriptProblem::Invalid);
    }
    let mut contents =
        Vec::with_capacity(usize::try_from(metadata.len()).map_err(|_| ScriptProblem::Invalid)?);
    file.read_to_end(&mut contents)
        .map_err(|_| ScriptProblem::Invalid)?;
    let lines = contents.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let has_marker = |marker: &str| {
        let expected = format!("# {marker}");
        lines.contains(&expected.as_bytes())
    };
    if !has_marker(SCRIPT_MARKER)
        || !has_marker(bridge.marker())
        || !transaction_helpers(&contents).is_some_and(|helpers| helpers_match(helpers, bridge))
    {
        return Err(ScriptProblem::Invalid);
    }
    Ok(ValidatedScript { contents })
}

fn transaction_helpers(contents: &[u8]) -> Option<&[u8]> {
    let begin = single_fragment_offset(contents, TRANSACTION_BEGIN)? + TRANSACTION_BEGIN.len();
    let end = single_fragment_offset(contents, TRANSACTION_END)?;
    (begin <= end).then_some(&contents[begin..end])
}

fn single_fragment_offset(contents: &[u8], fragment: &[u8]) -> Option<usize> {
    let mut matches = contents
        .windows(fragment.len())
        .enumerate()
        .filter_map(|(offset, candidate)| (candidate == fragment).then_some(offset));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// The canonical helper block for a bridge, as it must appear on a node
/// without a memory policy.  The scoped variant is matched structurally by
/// [`scoped_helpers_match`] instead, because its bridge names host paths.
fn direct_helpers() -> String {
    CANONICAL_SCOPED_HELPERS.replacen(SCOPED_SEND_BLOCK, DIRECT_SEND_BLOCK, 1)
}

fn helpers_match(candidate: &[u8], bridge: &LaunchBridge) -> bool {
    let Ok(candidate) = std::str::from_utf8(candidate) else {
        return false;
    };
    match bridge {
        LaunchBridge::Direct => candidate == direct_helpers(),
        LaunchBridge::Scoped { config_path } => {
            scoped_helpers_match(candidate, config_path.as_deref())
        }
    }
}

/// Matches a candidate block against Tron's canonical scoped block line by
/// line.  Exactly two things may differ: the `scoped_exec_command` line may
/// name this node's own atmux executable and configuration file, and the
/// `atmux-web` service-cap override may be absent or carry another byte count
/// (`scoped-exec` itself enforces the owner's cap policy at launch).
fn scoped_helpers_match(candidate: &str, config_path: Option<&Path>) -> bool {
    let Some(canonical_command_line) = SCOPED_SEND_BLOCK
        .lines()
        .find(|line| line.starts_with(SCOPED_COMMAND_PREFIX))
    else {
        return false;
    };
    let mut expected = CANONICAL_SCOPED_HELPERS.split('\n');
    let mut actual = candidate.split('\n').peekable();
    while let Some(line) = expected.next() {
        if line == canonical_command_line {
            let Some(bridge_line) = actual.next() else {
                return false;
            };
            if !scoped_command_line_is_this_node(bridge_line, config_path) {
                return false;
            }
        } else if line == SERVICE_OVERRIDE_IF_LINE {
            // Consume the canonical three-line override; the candidate may
            // omit it entirely or supply its own byte count.
            let (Some(cap), Some(end)) = (expected.next(), expected.next()) else {
                return false;
            };
            if !(cap.starts_with(SERVICE_OVERRIDE_CAP_PREFIX) && end == SERVICE_OVERRIDE_END_LINE) {
                return false;
            }
            if actual.peek() == Some(&SERVICE_OVERRIDE_IF_LINE) {
                actual.next();
                let (Some(cap), Some(end)) = (actual.next(), actual.next()) else {
                    return false;
                };
                if !(service_override_cap_line(cap) && end == SERVICE_OVERRIDE_END_LINE) {
                    return false;
                }
            }
        } else if actual.next() != Some(line) {
            return false;
        }
    }
    actual.next().is_none()
}

fn service_override_cap_line(line: &str) -> bool {
    line.strip_prefix(SERVICE_OVERRIDE_CAP_PREFIX)
        .and_then(|rest| rest.strip_suffix(SERVICE_OVERRIDE_CAP_SUFFIX))
        .is_some_and(|digits| !digits.is_empty() && digits.parse::<u64>().is_ok())
}

/// The bridge must run an atmux executable this user owns (or root installed)
/// through the daemon's own configuration file, so the roster enters exactly
/// the memory policy in effect.
fn scoped_command_line_is_this_node(line: &str, config_path: Option<&Path>) -> bool {
    let Some(inner) = line
        .strip_prefix(SCOPED_COMMAND_PREFIX)
        .and_then(|rest| rest.strip_suffix(SCOPED_COMMAND_SUFFIX))
    else {
        return false;
    };
    let Ok(words) = shell_words::split(inner) else {
        return false;
    };
    let [executable, config_flag, configured, verb] = words.as_slice() else {
        return false;
    };
    if config_flag != "--config" || verb != "scoped-exec" {
        return false;
    }
    let executable = Path::new(executable);
    let euid = geteuid().as_raw();
    let trusted_executable = executable.is_absolute()
        && validate_secure_ancestry(executable, euid).is_ok()
        && fs::symlink_metadata(executable).is_ok_and(|metadata| {
            metadata.is_file()
                && metadata.mode() & 0o111 != 0
                && metadata.mode() & 0o022 == 0
                && (metadata.uid() == euid || metadata.uid() == 0)
        });
    let same_config = config_path.is_some_and(|expected| {
        match (fs::canonicalize(configured), fs::canonicalize(expected)) {
            (Ok(actual), Ok(expected)) => actual == expected,
            _ => false,
        }
    });
    trusted_executable && same_config
}

fn validate_secure_ancestry(path: &Path, euid: u32) -> Result<(), ()> {
    if !path.is_absolute() {
        return Err(());
    }
    let mut current = path.parent().ok_or(())?;
    loop {
        let metadata = fs::symlink_metadata(current).map_err(|_| ())?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.mode() & 0o022 != 0
            || (metadata.uid() != 0 && metadata.uid() != euid)
        {
            return Err(());
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }
    Ok(())
}

fn acquire_runtime_lock(runtime_dir: &Path) -> Result<File, LockError> {
    let euid = geteuid().as_raw();
    let base = runtime_dir.parent().ok_or(LockError::Unsafe)?;
    validate_secure_runtime_directory(base, euid)?;
    match rustix::fs::mkdir(runtime_dir, Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(_) => return Err(LockError::Unsafe),
    }
    validate_secure_runtime_directory(runtime_dir, euid)?;
    let lock_path = runtime_dir.join(LOCK_FILE_NAME);
    let descriptor = rustix::fs::open(
        &lock_path,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|_| LockError::Unsafe)?;
    let lock = File::from(descriptor);
    let metadata = lock.metadata().map_err(|_| LockError::Unsafe)?;
    validate_lock_metadata(&metadata, euid)?;
    lock.try_lock_exclusive().map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            LockError::Busy
        } else {
            LockError::Unsafe
        }
    })?;
    Ok(lock)
}

fn validate_runtime_location(runtime_dir: &Path) -> Result<(), LockError> {
    let euid = geteuid().as_raw();
    let base = runtime_dir.parent().ok_or(LockError::Unsafe)?;
    validate_secure_runtime_directory(base, euid)?;
    match fs::symlink_metadata(runtime_dir) {
        Ok(_) => validate_secure_runtime_directory(runtime_dir, euid)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(LockError::Unsafe),
    }
    match fs::symlink_metadata(runtime_dir.join(LOCK_FILE_NAME)) {
        Ok(metadata) => validate_lock_metadata(&metadata, euid),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(LockError::Unsafe),
    }
}

fn validate_lock_metadata(metadata: &fs::Metadata, euid: u32) -> Result<(), LockError> {
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != euid
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        return Err(LockError::Unsafe);
    }
    Ok(())
}

fn validate_secure_runtime_directory(path: &Path, euid: u32) -> Result<(), LockError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| LockError::Unsafe)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != euid
        || metadata.mode() & 0o077 != 0
    {
        return Err(LockError::Unsafe);
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::Write,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(1);

    fn fixture_directory() -> PathBuf {
        let directory = std::env::current_dir().unwrap().join(format!(
            ".atmux-recovery-test-{}-{}",
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn write_script(path: &Path, helpers: &str, body: &str) {
        let mut file = File::create(path).unwrap();
        write!(
            file,
            "#!/usr/bin/env bash\n# {SCRIPT_MARKER}\n# ATMUX_QUICK_RESUME_TRANSACTION_BEGIN\n{helpers}# ATMUX_QUICK_RESUME_TRANSACTION_END\n{body}\n"
        )
        .unwrap();
        let mut permissions = file.metadata().unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    /// A direct-bridge fixture script in its own private directory.
    fn fixture_script(body: &str) -> PathBuf {
        let path = fixture_directory().join("resume.sh");
        write_script(&path, &direct_helpers(), body);
        path
    }

    /// A scoped-bridge fixture: the canonical Tron block rewritten to name a
    /// private executable and configuration file beside the script.
    fn scoped_fixture(body: &str) -> (PathBuf, LaunchBridge, String) {
        let directory = fixture_directory();
        let executable = directory.join("atmux");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let config = directory.join("config.toml");
        fs::write(&config, "").unwrap();
        let canonical_line = SCOPED_SEND_BLOCK
            .lines()
            .find(|line| line.starts_with(SCOPED_COMMAND_PREFIX))
            .unwrap();
        let this_node_line = format!(
            "{SCOPED_COMMAND_PREFIX}{} --config {} scoped-exec{SCOPED_COMMAND_SUFFIX}",
            executable.display(),
            config.display()
        );
        let helpers = CANONICAL_SCOPED_HELPERS.replacen(canonical_line, &this_node_line, 1);
        let path = directory.join("resume.sh");
        write_script(&path, &helpers, body);
        (
            path,
            LaunchBridge::Scoped {
                config_path: Some(config),
            },
            helpers,
        )
    }

    fn remove_fixture(path: &Path) {
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    async fn wait_until_finished(runner: &RecoveryRunner) -> RecoveryStatus {
        for _ in 0..100 {
            let status = runner.status().await;
            if status.phase != RecoveryPhase::Running {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("fixture recovery did not finish");
    }

    #[tokio::test]
    async fn single_flight_rejects_a_second_start() {
        let path = fixture_script("sleep 0.25");
        let runner = RecoveryRunner::fixture("tron", &path, Duration::from_secs(2));
        assert_eq!(runner.start().await.unwrap().phase, RecoveryPhase::Running);
        assert!(matches!(
            runner.start().await,
            Err(RecoveryStartError::Running)
        ));
        assert_eq!(
            wait_until_finished(&runner).await.phase,
            RecoveryPhase::Succeeded
        );
        remove_fixture(&path);
    }

    #[tokio::test]
    async fn file_lock_prevents_two_server_processes_from_running_recovery() {
        let path = fixture_script("sleep 0.4");
        let first = RecoveryRunner::fixture("tron", &path, Duration::from_secs(2));
        let second = RecoveryRunner::fixture("tron", &path, Duration::from_secs(2));
        first.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(
            second.start().await,
            Err(RecoveryStartError::Running)
        ));
        assert_eq!(
            wait_until_finished(&first).await.phase,
            RecoveryPhase::Succeeded
        );
        remove_fixture(&path);
    }

    #[tokio::test]
    async fn output_is_not_exposed_and_failure_is_bounded_to_an_exit_code() {
        let path =
            fixture_script("printf 'secret-output\\n'\nprintf 'secret-error\\n' >&2\nexit 7");
        let runner = RecoveryRunner::fixture("tron", &path, Duration::from_secs(2));
        runner.start().await.unwrap();
        let status = wait_until_finished(&runner).await;
        assert_eq!(status.phase, RecoveryPhase::Failed);
        assert_eq!(status.message, "Recovery script exited with status 7");
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains(".atmux-recovery-test"));
        remove_fixture(&path);
    }

    #[tokio::test]
    async fn the_script_sees_only_the_fixed_identity_environment() {
        let path = fixture_script(
            "[ \"$HOME\" = \"$ATMUX_EXPECTED_HOME\" ] && exit 9\nprintf '%s\\n' \"$HOME\" \"$USER\" \"$LOGNAME\" \"$PATH\" > \"$HOME/seen\"\n[ -z \"${ATMUX_LEAK:-}\" ] || exit 5\n",
        );
        let directory = path.parent().unwrap().to_path_buf();
        // SAFETY-free: these are process-wide test variables the child must
        // never inherit; the runner clears its environment before spawning.
        // They are read back only by this test.
        let runner = RecoveryRunner::fixture("tron", &path, Duration::from_secs(2));
        let mut command = Command::new(BASH_COMMAND);
        command.env("ATMUX_LEAK", "1");
        configure_script_environment(&mut command, &HostEnvironment::fixture(&directory));
        let status = command
            .arg("-c")
            .arg("[ -z \"${ATMUX_LEAK:-}\" ] && [ \"$USER\" = fixture ] && [ \"$LOGNAME\" = fixture ] && [ \"$HOME\" = \"$1\" ]")
            .arg("--")
            .arg(&directory)
            .status()
            .await
            .unwrap();
        assert!(status.success(), "the fixed environment was not applied");

        runner.start().await.unwrap();
        assert_eq!(
            wait_until_finished(&runner).await.phase,
            RecoveryPhase::Succeeded
        );
        let seen = fs::read_to_string(directory.join("seen")).unwrap();
        let mut lines = seen.lines();
        assert_eq!(lines.next(), Some(directory.to_str().unwrap()));
        assert_eq!(lines.next(), Some("fixture"));
        assert_eq!(lines.next(), Some("fixture"));
        let path_entries = lines.next().unwrap().split(':').collect::<Vec<_>>();
        assert!(path_entries.contains(&"/usr/bin"));
        assert!(
            path_entries
                .iter()
                .any(|entry| entry.ends_with(".local/bin"))
        );
        remove_fixture(&path);
    }

    #[tokio::test]
    async fn inherited_bash_env_cannot_run_before_the_pinned_script() {
        let path = fixture_script(":");
        let directory = path.parent().unwrap();
        let marker = directory.join("ambient-code-ran");
        let bash_env = directory.join("malicious-bash-env.sh");
        fs::write(
            &bash_env,
            format!(
                "printf injected > {}\n",
                shell_words::quote(&marker.display().to_string())
            ),
        )
        .unwrap();
        let mut command = Command::new(BASH_COMMAND);
        command.env("BASH_ENV", &bash_env);
        configure_script_environment(&mut command, &HostEnvironment::fixture(directory));
        let status = command.arg("-c").arg(":").status().await.unwrap();
        assert!(status.success());
        assert!(!marker.exists(), "BASH_ENV code ran before the fixture");
        remove_fixture(&path);
    }

    #[test]
    fn pinned_bash_is_absolute_and_executable_on_this_platform() {
        assert!(Path::new(BASH_COMMAND).is_absolute());
        assert!(executable_file(Path::new(BASH_COMMAND)));
    }

    #[test]
    fn sanitized_path_finds_tmux_and_the_user_launcher_directories() {
        let home = Path::new("/nonexistent/home");
        let path = recovery_path(home);
        let entries = path.split(':').collect::<Vec<_>>();
        assert!(entries.contains(&"/usr/bin"));
        assert!(entries.contains(&"/nonexistent/home/.local/bin"));
        assert!(entries.contains(&"/nonexistent/home/.asdf/shims"));
        assert_eq!(
            entries.contains(&"/opt/homebrew/bin"),
            cfg!(target_os = "macos")
        );
        assert_eq!(resolve_on_path("sh", "/nonexistent:/usr/bin:/bin"), {
            if executable_file(Path::new("/usr/bin/sh")) {
                Some(PathBuf::from("/usr/bin/sh"))
            } else {
                Some(PathBuf::from("/bin/sh"))
            }
        });
        assert!(resolve_on_path("definitely-missing-atmux-command", &path).is_none());
        assert!(!required_commands_available(
            &[PathBuf::from("/definitely/missing/atmux-recovery-command")],
            &path
        ));
        assert!(!required_commands_available(
            &[PathBuf::from("relative/command")],
            &path
        ));
    }

    #[tokio::test]
    async fn timeout_stops_a_hung_fixture() {
        let path = fixture_script("sleep 30");
        let runner = RecoveryRunner::fixture("tron", &path, Duration::from_secs(1));
        runner.start().await.unwrap();
        assert_eq!(
            wait_until_finished(&runner).await.phase,
            RecoveryPhase::TimedOut
        );
        remove_fixture(&path);
    }

    #[tokio::test]
    async fn timeout_terminates_and_reaps_background_descendants() {
        let path = fixture_script(":");
        let pid_file = path.parent().unwrap().join("descendant.pid");
        write_script(
            &path,
            &direct_helpers(),
            &format!(
                "(trap '' TERM; while :; do sleep 30; done) &\nprintf '%s' \"$!\" > {}\nwait",
                shell_words::quote(&pid_file.display().to_string()),
            ),
        );
        let runner = RecoveryRunner::fixture("tron", &path, Duration::from_secs(1));
        runner.start().await.unwrap();
        assert_eq!(
            wait_until_finished(&runner).await.phase,
            RecoveryPhase::TimedOut
        );
        let raw_pid = fs::read_to_string(&pid_file).unwrap();
        let pid = Pid::from_raw(raw_pid.trim().parse().unwrap()).unwrap();
        for _ in 0..50 {
            if rustix::process::test_kill_process(pid).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            rustix::process::test_kill_process(pid).is_err(),
            "the recovery timeout must not leave a descendant alive"
        );
        let _ = fs::remove_file(pid_file);
        remove_fixture(&path);
    }

    #[tokio::test]
    async fn missing_or_markerless_roster_fails_closed_with_a_pathless_message() {
        let detached = RecoveryRunner::detached("midnight");
        let status = detached.status().await;
        assert!(!status.available);
        assert_eq!(status.phase, RecoveryPhase::Unavailable);
        assert_eq!(
            status.message,
            "Quick Resume roster script is not installed on this machine"
        );
        assert!(matches!(
            detached.start().await,
            Err(RecoveryStartError::Unavailable(_))
        ));

        let path = fixture_script(":");
        fs::write(
            &path,
            format!("#!/usr/bin/env bash\n# {SCRIPT_MARKER}\nexit 0\n"),
        )
        .unwrap();
        let runner = RecoveryRunner::fixture("tron", &path, Duration::from_secs(1));
        let status = runner.status().await;
        assert!(!status.available);
        assert_eq!(
            status.message,
            "Quick Resume roster script fails its safety checks"
        );
        assert!(matches!(
            runner.start().await,
            Err(RecoveryStartError::Unavailable(_))
        ));
        remove_fixture(&path);
    }

    #[tokio::test]
    async fn production_runner_follows_the_configuration() {
        let directory = fixture_directory();
        let config_file = directory.join("config.toml");
        fs::write(&config_file, "").unwrap();
        let script = directory.join(DEFAULT_SCRIPT_NAME);
        write_script(&script, &direct_helpers(), ":");

        let mut config = Config::default();
        config.node.id = "clue".to_owned();
        config.source_path = Some(config_file.clone());
        let runner = RecoveryRunner::production(&config);
        assert_eq!(runner.inner.script, script);
        assert_eq!(runner.inner.bridge, LaunchBridge::Direct);
        let status = runner.status().await;
        assert_eq!(status.machine, "clue");
        assert!(
            status.available || status.message == LOCK_MESSAGE,
            "a direct roster beside the config is offered unless this host lacks a private runtime dir: {}",
            status.message
        );

        let explicit = directory.join("elsewhere.sh");
        write_script(&explicit, &direct_helpers(), ":");
        config.recovery.script = Some(explicit.clone());
        config.recovery.required_commands = vec![directory.join("missing-launcher")];
        let runner = RecoveryRunner::production(&config);
        assert_eq!(runner.inner.script, explicit);
        let status = runner.status().await;
        assert!(!status.available);
        assert_eq!(
            status.message,
            "Quick Resume roster commands are missing or not executable on this machine"
        );

        #[cfg(target_os = "linux")]
        {
            config.agent_resources.memory_max_bytes = Some(1024 * 1024 * 1024);
            let runner = RecoveryRunner::production(&config);
            assert_eq!(
                runner.inner.bridge,
                LaunchBridge::Scoped {
                    config_path: Some(config_file.clone())
                }
            );
            // A direct roster is refused where the memory policy demands the
            // scoped bridge.
            assert!(!runner.status().await.available);
        }
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn checked_in_direct_bridge_and_example_roster_validate_only_without_memory_policy() {
        let path = fixture_script(":");
        assert!(validate_script(&path, &LaunchBridge::Direct).is_ok());
        assert!(
            validate_script(
                &path,
                &LaunchBridge::Scoped {
                    config_path: Some(path.parent().unwrap().join("config.toml"))
                }
            )
            .is_err()
        );
        let direct = direct_helpers();
        assert!(direct.contains(&format!("# {DIRECT_EXEC_MARKER}")));
        assert!(
            !direct
                .lines()
                .any(|line| !line.starts_with('#') && line.contains("scoped-exec"))
        );
        assert!(direct.contains("\"exec $2\" Enter"));

        // The shipped example roster carries exactly this block.
        let example = fs::read_to_string(
            std::env::current_dir()
                .unwrap()
                .join("deploy/quick-resume/quick-resume.example.sh"),
        )
        .unwrap();
        let example_path = path.parent().unwrap().join("example.sh");
        fs::write(&example_path, &example).unwrap();
        fs::set_permissions(&example_path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(validate_script(&example_path, &LaunchBridge::Direct).is_ok());

        let valid = fs::read_to_string(&path).unwrap();
        let assert_invalid = |source: String| {
            fs::write(&path, source).unwrap();
            assert!(validate_script(&path, &LaunchBridge::Direct).is_err());
        };
        assert_invalid(valid.replace("\"exec $2\" Enter", "\"exec $2; rm -rf /\" Enter"));
        assert_invalid(valid.replace(&format!("# {DIRECT_EXEC_MARKER}\n"), ""));
        assert_invalid(valid.replace(
            DIRECT_SEND_BLOCK,
            &format!("if false; then\n{DIRECT_SEND_BLOCK}fi\n"),
        ));
        assert_invalid(valid.replace(
            "# ATMUX_QUICK_RESUME_TRANSACTION_END\n",
            &format!("{DIRECT_SEND_BLOCK}# ATMUX_QUICK_RESUME_TRANSACTION_END\n"),
        ));
        remove_fixture(&path);
    }

    #[test]
    fn checked_in_tron_bridge_requires_exact_active_transaction_helpers() {
        let (path, bridge, valid_helpers) = scoped_fixture(":");
        let block = fs::read_to_string(
            std::env::current_dir()
                .unwrap()
                .join("deploy/systemd/resume-tron-scoped-exec-block.bash"),
        )
        .unwrap();
        assert_eq!(block, SCOPED_SEND_BLOCK);
        assert!(CANONICAL_SCOPED_HELPERS.contains(&block));
        assert!(validate_script(&path, &bridge).is_ok());
        // Tron's live roster shape (its own paths) is what the canonical
        // fixture holds; on any other machine it fails closed because that
        // executable and configuration are not this node's.
        assert!(
            !scoped_helpers_match(
                CANONICAL_SCOPED_HELPERS,
                Some(path.parent().unwrap().join("config.toml").as_path())
            ) || Path::new("/home/ryan/.local/bin/atmux").exists()
        );

        let valid = fs::read_to_string(&path).unwrap();
        let assert_invalid = |source: String| {
            fs::write(&path, source).unwrap();
            assert!(validate_script(&path, &bridge).is_err());
        };
        let assert_valid = |source: String| {
            fs::write(&path, source).unwrap();
            assert!(validate_script(&path, &bridge).is_ok());
        };
        let override_block = "  if [ \"$unit_session\" = atmux-web ]; then\n    scoped_exec_command+=' --recovery-service-memory-max-bytes 60129542144'\n  fi\n";
        assert!(valid.contains(override_block));
        // The service cap is optional and policy-checked by scoped-exec.
        assert_valid(valid.replace(override_block, ""));
        assert_valid(valid.replace("60129542144", "51539607552"));
        assert_invalid(valid.replace("60129542144", "lots"));
        assert_invalid(valid.replace("60129542144", ""));

        let this_line = valid_helpers
            .lines()
            .find(|line| line.starts_with(SCOPED_COMMAND_PREFIX))
            .unwrap()
            .to_owned();
        assert_invalid(valid.replace(
            &this_line,
            &this_line.replace("scoped-exec'", "scoped-exec --memory-max-bytes 1'"),
        ));
        assert_invalid(valid.replace(
            &this_line,
            &this_line.replace("/atmux --config", "/missing-atmux --config"),
        ));
        assert_invalid(valid.replace(
            &this_line,
            &this_line.replace("/config.toml scoped-exec", "/other.toml scoped-exec"),
        ));
        assert_invalid(valid.replace(&this_line, "  local scoped_exec_command='/bin/sh -c'"));

        let commented = block.lines().fold(String::new(), |mut output, line| {
            output.push_str("# ");
            output.push_str(line);
            output.push('\n');
            output
        });
        let this_block = block.replacen(
            SCOPED_SEND_BLOCK
                .lines()
                .find(|line| line.starts_with(SCOPED_COMMAND_PREFIX))
                .unwrap(),
            &this_line,
            1,
        );
        assert!(valid.contains(&this_block));
        assert_invalid(valid.replace(&this_block, &commented));
        assert_invalid(valid.replace(&this_block, &format!("if false; then\n{this_block}fi\n")));
        assert_invalid(valid.replace(
            "\"exec $scoped_exec_command -- $2\" Enter",
            "\"exec $2\" Enter",
        ));
        assert_invalid(valid.replace(
            "# ATMUX_QUICK_RESUME_TRANSACTION_END\n",
            &format!("{this_block}# ATMUX_QUICK_RESUME_TRANSACTION_END\n"),
        ));
        assert_invalid(valid.replace(
            &this_block,
            &format!(
                "{this_block}scoped_exec_command+=' --recovery-service-memory-max-bytes 60129542144'\n"
            ),
        ));
        // A direct roster never satisfies a memory-scoped node.
        assert_invalid(
            valid
                .replace(&this_block, DIRECT_SEND_BLOCK)
                .replace(&format!("# {SCOPED_EXEC_MARKER}\n"), ""),
        );
        remove_fixture(&path);
    }

    #[tokio::test]
    async fn writable_or_symlinked_script_ancestry_fails_closed() {
        let path = fixture_script(":");
        let directory = path.parent().unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o770)).unwrap();
        let runner = RecoveryRunner::fixture("tron", &path, Duration::from_secs(1));
        assert!(!runner.status().await.available);
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();

        let link = directory.join("linked.sh");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let linked = RecoveryRunner::fixture("tron", &link, Duration::from_secs(1));
        assert!(!linked.status().await.available);
        remove_fixture(&path);
    }

    #[tokio::test]
    async fn unsafe_runtime_directory_fails_closed_before_spawn() {
        let path = fixture_script(":");
        let directory = path.parent().unwrap();
        let runtime_target = directory.join("runtime-target");
        fs::create_dir(&runtime_target).unwrap();
        fs::set_permissions(&runtime_target, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&runtime_target, directory.join("runtime")).unwrap();

        let runner = RecoveryRunner::fixture("tron", &path, Duration::from_secs(1));
        let status = runner.status().await;
        assert!(!status.available);
        assert_eq!(status.message, LOCK_MESSAGE);
        assert!(matches!(
            runner.start().await,
            Err(RecoveryStartError::Unavailable(_))
        ));
        remove_fixture(&path);
    }
}
