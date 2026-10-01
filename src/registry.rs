//! Opt-in durable session history. Native identities and bundles are peer-only;
//! browser/MCP responses use `SessionRecord`, which cannot contain an identity.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use flate2::{Compression, write::GzEncoder};
use fs2::FileExt;
use rmcp::schemars::JsonSchema;
use rustix::fs::{AtFlags, Dir, Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::{
    status::{AgentKind, AgentStatus},
    tmux::{self, Session},
    workspace,
};

const MAX_RECORD_BYTES: u64 = 64 * 1024;
const MAX_FILES: usize = 4096;
const MAX_DEPTH: usize = 32;
const PAGE_SIZE: usize = 16; // Even maximally sized records fit the 1 MiB peer transport.
const OBSERVATION_INTERVAL_MS: u64 = 30_000;
const DAY_MS: u64 = 86_400_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RegistryConfig {
    pub enabled: bool,
    /// Defaults to the platform state directory's `registry` subdirectory.
    pub directory: Option<PathBuf>,
    pub max_owner_records: usize,
    pub archived_retention_days: u64,
    /// Caps both the uncompressed native files and the resulting tar.gz.
    pub bundle_max_bytes: u64,
    pub bundle_quota_bytes: u64,
    pub restore_on_start: bool,
    pub restore_machines: Vec<String>,
}
impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: None,
            max_owner_records: 10_000,
            archived_retention_days: 90,
            bundle_max_bytes: 256 * 1024 * 1024,
            bundle_quota_bytes: 4 * 1024 * 1024 * 1024,
            restore_on_start: false,
            restore_machines: Vec::new(),
        }
    }
}
impl RegistryConfig {
    /// # Errors
    /// Rejects invalid limits and relative storage paths even before enabling.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.restore_on_start || (self.enabled && !self.restore_machines.is_empty()),
            "restore_on_start requires registry.enabled and a machine allowlist"
        );
        ensure!(
            self.restore_machines.len() <= 64,
            "restore machine allowlist is too large"
        );
        for machine in &self.restore_machines {
            crate::machine::validate_machine_id(machine)?;
        }
        ensure!(
            (1..=100_000).contains(&self.max_owner_records),
            "registry record count must be 1..=100000"
        );
        ensure!(
            (1..=3650).contains(&self.archived_retention_days),
            "registry retention must be 1..=3650 days"
        );
        ensure!(
            (64 * 1024..=4 * 1024 * 1024 * 1024).contains(&self.bundle_max_bytes),
            "registry bundle limit must be 64 KiB..=4 GiB"
        );
        ensure!(
            self.bundle_quota_bytes >= self.bundle_max_bytes,
            "registry bundle quota must fit one bundle"
        );
        ensure!(
            self.directory.as_ref().is_none_or(|p| p.is_absolute()),
            "registry directory must be absolute"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    #[default]
    Running,
    Exited,
    Closed,
    Archived,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct ProjectPosition {
    pub remote: Option<String>,
    pub root: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub dirty: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct BundleInfo {
    #[serde(default = "available_by_default")]
    pub available: bool,
    pub id: String,
    pub bytes: u64,
    pub sha256: String,
    pub native_log: bool,
}

fn available_by_default() -> bool {
    true
}
impl Default for BundleInfo {
    fn default() -> Self {
        Self {
            available: true,
            id: String::new(),
            bytes: 0,
            sha256: String::new(),
            native_log: false,
        }
    }
}

/// Public projection. Native provider ids, config roots, and log paths have
/// no fields in this type; it is shared by every browser/MCP history response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct SessionRecord {
    pub session_key: String,
    pub machine: String,
    pub name: String,
    pub description: Option<String>,
    pub description_source: Option<String>,
    pub title: String,
    pub harness: String,
    pub profile: String,
    pub mode: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
    pub cwd: String,
    pub project: ProjectPosition,
    pub created_ms: u64,
    pub last_active_ms: u64,
    pub last_seen_ms: u64,
    pub closed_ms: Option<u64>,
    pub archived_ms: Option<u64>,
    pub state: SessionState,
    pub last_status: String,
    pub needs_input_reason: Option<String>,
    /// A2 may attach its durable digest id/version here without native identity.
    pub digest_ref: Option<String>,
    pub bundle: Option<BundleInfo>,
    pub archive_error: Option<String>,
}
impl SessionRecord {
    fn bound(&mut self) {
        for value in [&mut self.name, &mut self.title, &mut self.profile] {
            truncate(value, 512);
        }
        for value in [&mut self.cwd, &mut self.project.root] {
            truncate(value, 4096);
        }
        for value in [&mut self.description, &mut self.project.remote]
            .into_iter()
            .flatten()
        {
            truncate(value, 2048);
        }
        for value in [
            &mut self.description_source,
            &mut self.mode,
            &mut self.model,
            &mut self.effort,
            &mut self.service_tier,
            &mut self.project.branch,
            &mut self.project.head,
            &mut self.needs_input_reason,
            &mut self.digest_ref,
            &mut self.archive_error,
        ]
        .into_iter()
        .flatten()
        {
            truncate(value, 512);
        }
        truncate(&mut self.harness, 32);
        truncate(&mut self.last_status, 64);
        self.project.remote = self
            .project
            .remote
            .take()
            .and_then(|v| credential_free_remote(&v));
    }
}
fn truncate(value: &mut String, max: usize) {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

/// Never appears in `SessionRecord` or history tools. Peer transfer is guarded
/// separately from proxy/browser authentication in `registry_web`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct NativeIdentity {
    pub config_root: PathBuf,
    pub session_id: String,
    pub log_path: PathBuf,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct StoredRecord {
    pub record: SessionRecord,
    pub native: Option<NativeIdentity>,
    pub revision: u64,
    pub pane_id: String,
    pub instance_id: String,
    pub agent_pid: Option<u32>,
    pub process_start: Option<String>,
    pub owner_boot_id: Option<String>,
    pub server_id: Option<String>,
    pub desired_running: Option<bool>,
    pub close_reason: Option<String>,
    pub resume_generation: u64,
}
impl StoredRecord {
    pub(crate) fn validate(&mut self) -> Result<()> {
        ensure!(
            self.process_start
                .as_ref()
                .is_none_or(|value| value.len() <= 256 && !value.chars().any(char::is_control)),
            "invalid process start stamp"
        );
        ensure!(
            self.owner_boot_id
                .as_ref()
                .is_none_or(|value| tmux::valid_session_key(value)),
            "invalid owner boot id"
        );
        ensure!(
            self.server_id
                .as_ref()
                .is_none_or(|value| value.len() <= 256 && !value.chars().any(char::is_control)),
            "invalid server id"
        );
        ensure!(
            self.close_reason
                .as_deref()
                .is_none_or(|value| matches!(value, "user" | "disappeared" | "node_loss")),
            "invalid close intent"
        );
        ensure!(
            tmux::valid_session_key(&self.record.session_key),
            "invalid registry session key"
        );
        crate::machine::validate_machine_id(&self.record.machine)?;
        truncate(&mut self.pane_id, 64);
        truncate(&mut self.instance_id, 128);
        self.record.bound();
        if let Some(native) = &self.native {
            ensure!(
                native.config_root.is_absolute() && native.log_path.is_absolute(),
                "invalid native paths"
            );
            ensure!(
                native.config_root.as_os_str().len() <= 4096
                    && native.log_path.as_os_str().len() <= 8192,
                "oversized native paths"
            );
            ensure!(
                native.session_id.len() <= 128
                    && native
                        .session_id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "invalid native id"
            );
            ensure!(
                native.log_path.starts_with(&native.config_root),
                "native log outside config root"
            );
        }
        if let Some(bundle) = &self.record.bundle {
            ensure!(
                bundle.sha256.len() == 64
                    && bundle
                        .sha256
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "invalid bundle checksum"
            );
            ensure!(bundle.id == bundle.sha256, "invalid bundle id");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SessionsSearch {
    pub state: Option<SessionState>,
    pub machine: Option<String>,
    /// Case-insensitive substring of the project remote or root.
    pub project: Option<String>,
    /// Case-insensitive substring of name, description, or title.
    pub text: Option<String>,
    /// Stable UUID key of the last row in the preceding page.
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionsPage {
    pub sessions: Vec<SessionRecord>,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionGet {
    pub session_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RegistryPage {
    pub cursor: String,
    pub reset: bool,
    pub more: bool,
    pub records: Vec<StoredRecord>,
}

/// A1 integration seam: one callback site, after a durable transition.
#[derive(Clone, Debug)]
pub struct LifecycleEvent {
    pub event_type: &'static str,
    pub record: SessionRecord,
}
pub type EventSink = Arc<dyn Fn(LifecycleEvent) + Send + Sync>;

#[derive(Default)]
struct RegistryState {
    records: BTreeMap<String, StoredRecord>,
    observed: BTreeMap<String, (u64, u64)>, // output hash, last enrichment time
    revision: u64,
    sink: Option<EventSink>,
    owner_server: Option<String>,
    server_observed: bool,
    owner_boot_id: String,
}

pub struct Registry {
    config: RegistryConfig,
    owner: String,
    records_dir: File,
    bundles_dir: File,
    intents_dir: File,
    owner_lock: File,
    epoch: String,
    state: Mutex<RegistryState>,
    changed: watch::Sender<u64>,
    archive_gate: tokio::sync::Semaphore,
    resume_gate: Mutex<()>,
}
impl Drop for Registry {
    fn drop(&mut self) {
        // Explicitly release the flock even if a concurrent subprocess fork
        // briefly inherited its open description before exec/CLOEXEC.
        let _ = FileExt::unlock(&self.owner_lock);
    }
}
impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}
impl Registry {
    /// Opens private storage and recovers complete records, ignoring only
    /// uncommitted `.tmp` files. A corrupt committed record fails startup.
    /// # Errors
    /// Returns filesystem, lock, configuration, or committed-record errors.
    pub fn open(config: &RegistryConfig, owner: &str) -> Result<Option<Arc<Self>>> {
        config.validate()?;
        if !config.enabled {
            return Ok(None);
        }
        crate::machine::validate_machine_id(owner)?;
        let root = if let Some(root) = &config.directory {
            root.clone()
        } else {
            let dirs = directories::ProjectDirs::from("dev", "ryanmurf", "atmux")
                .context("state directory unavailable")?;
            dirs.state_dir()
                .unwrap_or_else(|| dirs.data_local_dir())
                .join("registry")
        };
        private_directory(&root)?;
        let root_fd = workspace::open_absolute_directory(&root)?;
        let lock = open_at(
            &root_fd,
            "registry.lock",
            OFlags::RDWR | OFlags::CREATE,
            0o600,
        )?;
        lock.try_lock_exclusive()
            .context("registry is already owned by another process")?;
        private_directory(&root.join("records"))?;
        private_directory(&root.join("bundles"))?;
        private_directory(&root.join("close-intents"))?;
        let intents_dir = workspace::open_absolute_directory(&root.join("close-intents"))?;
        let records_dir = workspace::open_absolute_directory(&root.join("records"))?;
        let bundles_dir = workspace::open_absolute_directory(&root.join("bundles"))?;
        let mut abandoned = Dir::read_from(&bundles_dir)?;
        while let Some(entry) = abandoned.read() {
            let entry = entry?;
            if let Ok(name) = entry.file_name().to_str()
                && name.strip_suffix(".tmp").is_some()
            {
                rustix::fs::unlinkat(&bundles_dir, name, AtFlags::empty())?;
            }
        }
        let mut state = RegistryState::default();
        let mut dir = Dir::read_from(&records_dir)?;
        while let Some(entry) = dir.read() {
            let entry = entry?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if name.strip_suffix(".tmp").is_some() {
                rustix::fs::unlinkat(&records_dir, name, AtFlags::empty())?;
                continue;
            }
            let Some(key) = name.strip_suffix(".json") else {
                continue;
            };
            ensure!(
                tmux::valid_session_key(key),
                "unexpected registry record filename"
            );
            let bytes = read_at(&records_dir, name, MAX_RECORD_BYTES)?;
            let mut stored: StoredRecord = serde_json::from_slice(&bytes)?;
            stored.validate()?;
            ensure!(
                stored.record.session_key == key,
                "registry filename/key mismatch"
            );
            state.revision = state.revision.max(stored.revision);
            state.records.insert(key.to_owned(), stored);
        }
        let epoch = tmux::new_session_key()?;
        state.owner_boot_id.clone_from(&epoch);
        let (changed, _) = watch::channel(state.revision);
        Ok(Some(Arc::new(Self {
            config: config.clone(),
            owner: owner.to_owned(),
            records_dir,
            bundles_dir,
            intents_dir,
            owner_lock: lock,
            epoch,
            state: Mutex::new(state),
            changed,
            archive_gate: tokio::sync::Semaphore::new(1),
            resume_gate: Mutex::new(()),
        })))
    }

    /// # Errors
    /// Returns an error for invalid search bounds/cursors.
    pub fn search(&self, query: &SessionsSearch) -> Result<SessionsPage> {
        validate_search(query)?;
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let limit = query.limit.unwrap_or(50).clamp(1, 100);
        let matches = |record: &SessionRecord| {
            query.state.is_none_or(|v| v == record.state)
                && query.machine.as_ref().is_none_or(|v| v == &record.machine)
                && query.project.as_ref().is_none_or(|v| {
                    contains(&record.project.root, v)
                        || contains(record.project.remote.as_deref().unwrap_or_default(), v)
                })
                && query.text.as_ref().is_none_or(|v| {
                    contains(&record.name, v)
                        || contains(record.description.as_deref().unwrap_or_default(), v)
                        || contains(&record.title, v)
                })
        };
        let mut sessions = state
            .records
            .values()
            .map(|s| &s.record)
            .filter(|r| query.cursor.as_ref().is_none_or(|c| &r.session_key > c) && matches(r))
            .take(limit + 1)
            .cloned()
            .collect::<Vec<_>>();
        let more = sessions.len() > limit;
        sessions.truncate(limit);
        let next_cursor = sessions
            .last()
            .filter(|_| more)
            .map(|record| record.session_key.clone());
        for record in &mut sessions {
            self.project_bundle_availability(record);
        }
        Ok(SessionsPage {
            sessions,
            next_cursor,
        })
    }

    /// # Errors
    /// Rejects malformed keys. Missing records return `None`.
    pub fn get(&self, key: &str) -> Result<Option<SessionRecord>> {
        ensure!(tmux::valid_session_key(key), "invalid session key");
        let mut record = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .records
            .get(key)
            .map(|s| s.record.clone());
        if let Some(record) = &mut record {
            self.project_bundle_availability(record);
        }
        Ok(record)
    }
    fn project_bundle_availability(&self, record: &mut SessionRecord) {
        if let Some(info) = &mut record.bundle {
            info.available = info.available && self.bundle_present(info);
        }
    }
    fn bundle_present(&self, info: &BundleInfo) -> bool {
        open_at(
            &self.bundles_dir,
            format!("{}.tar.gz", info.id),
            OFlags::RDONLY,
            0,
        )
        .is_ok_and(|file| {
            file.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.len() == info.bytes)
        })
    }

    /// Owner/coordinator integration seam for A4. Never serialize this through history APIs.
    /// # Errors
    /// Rejects malformed keys.
    pub fn native_identity(&self, key: &str) -> Result<Option<NativeIdentity>> {
        ensure!(tmux::valid_session_key(key), "invalid session key");
        Ok(self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .records
            .get(key)
            .and_then(|s| s.native.clone()))
    }
    /// A2 can persist its digest reference without exposing a provider identity.
    /// # Errors
    /// Rejects malformed keys, unknown records, oversized references or storage errors.
    pub fn set_digest_reference(&self, key: &str, reference: Option<String>) -> Result<()> {
        ensure!(
            tmux::valid_session_key(key) && reference.as_ref().is_none_or(|r| r.len() <= 512),
            "invalid digest reference"
        );
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stored = state
            .records
            .get(key)
            .cloned()
            .context("unknown registry session")?;
        if stored.record.digest_ref == reference {
            return Ok(());
        }
        stored.record.digest_ref = reference;
        self.commit(&mut state, stored)
    }

    /// A1 can attach a precise prompt reason to the durable waiting record.
    /// # Errors
    /// Rejects unknown reasons/keys and persistence failures.
    pub fn set_needs_input_reason(&self, key: &str, reason: Option<String>) -> Result<()> {
        ensure!(tmux::valid_session_key(key), "invalid session key");
        ensure!(
            reason.as_deref().is_none_or(|value| matches!(
                value,
                "idle_prompt" | "question" | "permission" | "startup_prompt" | "plan_approval"
            )),
            "invalid needs-input reason"
        );
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stored = state
            .records
            .get(key)
            .cloned()
            .context("unknown registry session")?;
        if stored.record.needs_input_reason == reason {
            return Ok(());
        }
        stored.record.needs_input_reason = reason;
        self.commit(&mut state, stored)
    }

    pub fn set_event_sink(&self, sink: EventSink) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sink = Some(sink);
    }

    /// Successful scans only: an unavailable tmux server must never close a
    /// registry record. Native identity is refreshed while the CLI still lives.
    /// # Errors
    /// Returns persistence errors; previously committed records remain valid.
    #[allow(clippy::too_many_lines)] // Keeps durable scan/close/archive transitions in one transaction boundary.
    pub fn observe(&self, sessions: &[Session], at_ms: u64) -> Result<()> {
        self.observe_inner(sessions, at_ms, None, false).map(|_| ())
    }

    #[allow(clippy::needless_pass_by_value)] // The tmux query returns an owned optional stamp.
    pub(crate) fn observe_owner(
        &self,
        sessions: &[Session],
        at_ms: u64,
        server: Option<String>,
    ) -> Result<bool> {
        self.observe_inner(sessions, at_ms, server.as_deref(), true)
    }

    #[allow(clippy::too_many_lines)]
    fn observe_inner(
        &self,
        sessions: &[Session],
        at_ms: u64,
        owner_server: Option<&str>,
        track_owner: bool,
    ) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.consume_close_intents(&mut state, at_ms)?;
        let mut seen = BTreeSet::new();
        let server_changed =
            track_owner && state.server_observed && state.owner_server.as_deref() != owner_server;
        if server_changed {
            state.owner_boot_id = tmux::new_session_key()?;
        }
        let owner_boot_id = state.owner_boot_id.clone();
        if track_owner {
            state.owner_server = owner_server.map(str::to_owned);
            state.server_observed = true;
        }
        let mut events = Vec::new();
        for session in sessions {
            let Some(key) = session
                .session_key
                .as_ref()
                .filter(|k| tmux::valid_session_key(k))
            else {
                continue;
            };
            seen.insert(key.clone());
            let previous = state.records.get(key).cloned();
            if previous.as_ref().is_some_and(|record| {
                record.close_reason.as_deref() == Some("user")
                    && record.instance_id == session.pane_identity
                    && record.agent_pid == session.agent_pid
            }) {
                continue;
            }

            if previous.is_none() && session.agent == AgentKind::Other {
                continue;
            }
            if previous.is_none() {
                self.retain(&mut state, at_ms, true)?;
                if state
                    .records
                    .values()
                    .filter(|s| s.record.machine == self.owner)
                    .count()
                    >= self.config.max_owner_records
                {
                    continue; // Active records are never evicted to admit another pane.
                }
            }
            let mut stored = previous.clone().unwrap_or_default();
            let process_changed =
                session.agent_pid.is_some() && stored.agent_pid != session.agent_pid;
            if process_changed {
                stored.native = None;
            }
            stored.pane_id.clone_from(&session.pane_id);
            stored.instance_id.clone_from(&session.pane_identity);
            if session.agent_pid.is_some() {
                stored.agent_pid = session.agent_pid;
                stored.process_start = crate::control::native_process_start_stamp(
                    session.agent_pid.unwrap_or_default(),
                );
            }
            if track_owner {
                stored.owner_boot_id = Some(owner_boot_id.clone());
                stored.server_id = owner_server.map(str::to_owned);
            }
            let record = &mut stored.record;
            record.session_key.clone_from(key);
            record.machine.clone_from(&self.owner);
            record.name.clone_from(&session.name);
            if record.description != session.description {
                record.description.clone_from(&session.description);
                record.description_source = record.description.as_ref().map(|_| "user".to_owned());
            }
            record.title.clone_from(&session.title);
            record.cwd = session.path.to_string_lossy().into_owned();
            if session.agent != AgentKind::Other {
                harness(session.agent).clone_into(&mut record.harness);
                record.profile.clone_from(&session.profile);
            }
            if record.created_ms == 0 {
                record.created_ms = at_ms;
            }
            record.last_active_ms = record.last_active_ms.max(
                session
                    .output_activity
                    .max(session.activity)
                    .saturating_mul(1000)
                    .min(at_ms),
            );
            session.status.label().clone_into(&mut record.last_status);
            if session.status == AgentStatus::Waiting {
                if record.needs_input_reason.is_none() {
                    record.needs_input_reason = Some("idle_prompt".to_owned());
                }
            } else {
                record.needs_input_reason = None;
            }
            record.state = if session.agent == AgentKind::Other || session.agent_pid.is_none() {
                SessionState::Exited
            } else {
                SessionState::Running
            };
            record.closed_ms = None;
            stored.desired_running = Some(record.state == SessionState::Running);
            stored.close_reason = None;
            record.archived_ms = None;
            record.archive_error = None;
            let old_observed = state.observed.get(key).copied();
            let enrich = process_changed
                || old_observed
                    .is_none_or(|(_, at)| at_ms.saturating_sub(at) >= OBSERVATION_INTERVAL_MS);
            if enrich {
                record.project = git_position(&session.path);
                if session.pane_pid != 0 && session.agent_pid.is_some() {
                    stored.native =
                        crate::transcript::native_resume_target(session).map(|target| {
                            NativeIdentity {
                                config_root: target.config_dir,
                                session_id: target.session_id,
                                log_path: target.log_path,
                            }
                        });
                }
                if session.pane_pid != 0
                    && let Ok(Some(mode)) = tmux::Tmux::recorded_pane_mode(&session.pane_id)
                {
                    record.mode = Some(mode.id);
                    record.model = Some(mode.model);
                    record.effort = mode.effort;
                    record.service_tier = mode.service_tier;
                }
            }
            if old_observed.is_some_and(|(hash, _)| hash != session.content_hash) {
                record.last_active_ms = at_ms;
            }
            // Heartbeat checkpoints are bounded to the enrichment interval.
            if enrich || previous.as_ref().is_none_or(|p| p.record != *record) {
                record.last_seen_ms = at_ms;
            }
            record.bound();
            if previous.as_ref().is_none_or(|p| {
                p.record != stored.record
                    || p.native != stored.native
                    || p.agent_pid != stored.agent_pid
                    || p.instance_id != stored.instance_id
                    || p.owner_boot_id != stored.owner_boot_id
                    || p.server_id != stored.server_id
                    || p.process_start != stored.process_start
                    || p.desired_running != stored.desired_running
                    || p.close_reason != stored.close_reason
            }) {
                self.commit(&mut state, stored)?;
            }
            let enriched_at = if enrich {
                at_ms
            } else {
                old_observed.map_or(at_ms, |(_, at)| at)
            };
            state
                .observed
                .insert(key.clone(), (session.content_hash, enriched_at));
        }
        let missing = state
            .records
            .values()
            .filter(|s| {
                s.record.machine == self.owner
                    && !seen.contains(&s.record.session_key)
                    && s.record.state != SessionState::Archived
            })
            .map(|s| s.record.session_key.clone())
            .collect::<Vec<_>>();
        for key in missing {
            let mut stored = state.records[&key].clone();
            if stored.record.closed_ms.is_none() {
                let node_loss = track_owner
                    && (stored.owner_boot_id.as_deref() != Some(&owner_boot_id)
                        || stored.server_id.as_deref() != owner_server);
                let desired = stored
                    .desired_running
                    .unwrap_or(stored.record.state == SessionState::Running);
                stored.close_reason = Some(
                    if node_loss && desired {
                        "node_loss"
                    } else {
                        "disappeared"
                    }
                    .to_owned(),
                );
                stored.desired_running = Some(node_loss && desired);
                stored.record.state = SessionState::Closed;
                stored.record.closed_ms = Some(at_ms);
                let final_git = git_position(Path::new(&stored.record.cwd));
                if !final_git.root.is_empty() {
                    stored.record.project = final_git;
                }
                self.commit(&mut state, stored.clone())?;
                events.push(LifecycleEvent {
                    event_type: "session.closed",
                    record: stored.record.clone(),
                });
            }
            // Retry rejected/missing bundles only once per interval.
            if state
                .observed
                .get(&key)
                .is_some_and(|(_, at)| at_ms.saturating_sub(*at) < OBSERVATION_INTERVAL_MS)
                && stored.record.archive_error.is_some()
            {
                continue;
            }
            stored.record.state = SessionState::Archived;
            stored.record.archived_ms = Some(at_ms);
            stored.record.archive_error = None;
            let creation = self
                .archive_gate
                .try_acquire()
                .map_err(anyhow::Error::from)
                .and_then(|_permit| {
                    let evicted = self.evict_bundles_to(
                        &state,
                        self.config.bundle_quota_bytes - self.config.bundle_max_bytes,
                    )?;
                    self.mark_owner_evictions(&mut state, &evicted)?;
                    self.create_bundle(&stored)
                });
            match creation {
                Ok(info) => {
                    stored.record.bundle = Some(info);
                    self.commit(&mut state, stored.clone())?;
                    events.push(LifecycleEvent {
                        event_type: "session.archived",
                        record: stored.record,
                    });
                }
                Err(error) => {
                    // Do not reflect provider roots/ids or arbitrary OS messages.
                    eprintln!(
                        "atmux archive {} failed: {error:#}",
                        stored.record.session_key
                    );
                    stored.record.state = SessionState::Closed;
                    stored.record.archived_ms = None;
                    stored.record.archive_error =
                        Some("bundle unavailable; owner will retry".to_owned());
                    self.commit(&mut state, stored)?;
                }
            }
            state.observed.insert(key, (0, at_ms));
        }
        let retained = state.records.keys().cloned().collect::<BTreeSet<_>>();
        state.observed.retain(|key, _| retained.contains(key));
        self.retain(&mut state, at_ms, false)?;
        let evicted = self.evict_bundles_to(&state, self.config.bundle_quota_bytes)?;
        self.mark_owner_evictions(&mut state, &evicted)?;
        let sink = state.sink.clone();
        drop(state);
        // Exactly one event emission call site for the lead/A1 to wire.
        if let Some(sink) = sink {
            for event in events {
                sink(event);
            }
        }
        Ok(server_changed)
    }

    fn commit(&self, state: &mut RegistryState, mut stored: StoredRecord) -> Result<()> {
        stored.validate()?;
        let revision = state
            .revision
            .checked_add(1)
            .context("registry cursor exhausted")?;
        stored.revision = revision;
        let bytes = serde_json::to_vec(&stored)?;
        ensure!(
            bytes.len() as u64 <= MAX_RECORD_BYTES,
            "registry record too large"
        );
        atomic_write(
            &self.records_dir,
            &format!("{}.json", stored.record.session_key),
            &bytes,
        )?;
        state
            .records
            .insert(stored.record.session_key.clone(), stored);
        state.revision = revision;
        self.changed.send_replace(revision);
        Ok(())
    }

    fn retain(&self, state: &mut RegistryState, at_ms: u64, make_room: bool) -> Result<()> {
        let mut archived = state
            .records
            .values()
            .filter(|s| s.record.machine == self.owner && s.record.state == SessionState::Archived)
            .map(|s| {
                (
                    s.record.archived_ms.unwrap_or(0),
                    s.record.session_key.clone(),
                )
            })
            .collect::<Vec<_>>();
        archived.sort();
        let mut count = state
            .records
            .values()
            .filter(|s| s.record.machine == self.owner)
            .count();
        for (archived_ms, key) in archived {
            if at_ms.saturating_sub(archived_ms) <= self.config.archived_retention_days * DAY_MS
                && count < self.config.max_owner_records + usize::from(!make_room)
            {
                continue;
            }
            rustix::fs::unlinkat(&self.records_dir, format!("{key}.json"), AtFlags::empty())?;
            self.records_dir.sync_all()?;
            state.records.remove(&key);
            state.observed.remove(&key);
            count -= 1;
        }
        Ok(())
    }

    /// Peer-only incremental feed. The boot epoch forces a complete replay
    /// after restarts, so deletion/retention cannot invalidate a cursor silently.
    /// # Errors
    /// Rejects malformed or oversized cursors.
    pub async fn changes(&self, after: Option<&str>, wait_ms: u64) -> Result<RegistryPage> {
        let mut changed = self.changed.subscribe();
        let page = self.page(after)?;
        if !page.reset && page.records.is_empty() && wait_ms > 0 {
            let _ = tokio::time::timeout(
                Duration::from_millis(wait_ms.min(10_000)),
                changed.changed(),
            )
            .await;
        }
        self.page(after)
    }
    fn page(&self, after: Option<&str>) -> Result<RegistryPage> {
        let (epoch, revision) = match after {
            Some(value) => {
                ensure!(value.len() <= 100, "invalid registry cursor");
                let (epoch, revision) = value.split_once(':').context("invalid registry cursor")?;
                ensure!(
                    tmux::valid_session_key(epoch),
                    "invalid registry cursor epoch"
                );
                (
                    epoch,
                    revision
                        .parse::<u64>()
                        .context("invalid registry cursor revision")?,
                )
            }
            None => ("", 0),
        };
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reset = epoch != self.epoch || revision > state.revision;
        let revision = if reset { 0 } else { revision };
        let mut matches = state
            .records
            .values()
            .filter(|s| s.record.machine == self.owner && s.revision > revision)
            .collect::<Vec<_>>();
        matches.sort_by_key(|s| s.revision);
        let more = matches.len() > PAGE_SIZE;
        let records = matches
            .into_iter()
            .take(PAGE_SIZE)
            .cloned()
            .collect::<Vec<_>>();
        let next = if more {
            records.last().map_or(revision, |s| s.revision)
        } else {
            state.revision
        };
        Ok(RegistryPage {
            cursor: format!("{}:{next}", self.epoch),
            reset,
            more,
            records,
        })
    }

    /// Imports one authenticated owner's page. Coordinator records are never
    /// evicted. Older observations from a previous owner cannot undo a resume.
    /// # Errors
    /// Rejects spoofed owners, oversized pages, and unsafe identities.
    pub fn import_page(&self, machine: &str, page: &RegistryPage) -> Result<()> {
        ensure!(page.records.len() <= PAGE_SIZE, "oversized registry page");
        let mut checked = page.records.clone();
        for stored in &mut checked {
            stored.validate()?;
            ensure!(
                stored.record.machine == machine && machine != self.owner,
                "registry owner mismatch"
            );
            if let Some(info) = &stored.record.bundle {
                ensure!(
                    info.bytes <= self.config.bundle_max_bytes,
                    "oversized peer bundle"
                );
            }
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for stored in checked {
            let existing = state.records.get(&stored.record.session_key);
            if existing.is_some_and(|e| {
                e.resume_generation > stored.resume_generation
                    || (e.resume_generation == stored.resume_generation
                        && e.record.machine != machine
                        && e.record.last_seen_ms > stored.record.last_seen_ms)
            }) {
                continue;
            }
            if existing.is_none_or(|e| e.record != stored.record || e.native != stored.native) {
                self.commit(&mut state, stored)?;
            }
        }
        Ok(())
    }

    /// Owner/coordinator-only descriptor for an existing checksummed bundle.
    /// # Errors
    /// Rejects missing or invalid bundle files.
    pub fn bundle_file(&self, key: &str) -> Result<(File, BundleInfo)> {
        let record = self.get(key)?.context("unknown registry session")?;
        let info = record.bundle.context("session has no bundle")?;
        self.bundle_file_info(&info)
    }
    pub(crate) fn bundle_file_info(&self, info: &BundleInfo) -> Result<(File, BundleInfo)> {
        ensure!(
            info.id == info.sha256
                && info.id.len() == 64
                && info.id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid bundle identity"
        );
        let mut file = open_at(
            &self.bundles_dir,
            format!("{}.tar.gz", info.id),
            OFlags::RDONLY,
            0,
        )?;
        ensure!(
            file.metadata()?.len() == info.bytes && info.bytes <= self.config.bundle_max_bytes,
            "invalid bundle size"
        );
        ensure!(
            hash_reader(&mut file, self.config.bundle_max_bytes)?.0 == info.sha256,
            "bundle checksum mismatch"
        );
        file.seek(SeekFrom::Start(0))?;
        Ok((file, info.clone()))
    }
    pub(crate) fn missing_bundles(&self, page: &RegistryPage) -> Vec<(String, BundleInfo)> {
        page.records
            .iter()
            .filter_map(|s| {
                let info = s.record.bundle.as_ref()?;
                if !info.available {
                    return None;
                }
                // Existing descriptors may be stale; a checksum check retries damage.
                self.bundle_file(&s.record.session_key)
                    .is_err()
                    .then(|| (s.record.session_key.clone(), info.clone()))
            })
            .collect()
    }
    pub(crate) fn incoming_file(&self) -> Result<(String, File)> {
        let name = format!("{}.tmp", tmux::new_session_key()?);
        Ok((
            name.clone(),
            open_at(
                &self.bundles_dir,
                &name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
                0o600,
            )?,
        ))
    }
    pub(crate) fn discard_incoming(&self, name: &str) {
        let _ = rustix::fs::unlinkat(&self.bundles_dir, name, AtFlags::empty());
    }
    pub(crate) fn finish_incoming(&self, name: &str, info: &BundleInfo) -> Result<()> {
        let mut file = open_at(&self.bundles_dir, name, OFlags::RDONLY, 0)?;
        let (hash, bytes) = hash_reader(&mut file, self.config.bundle_max_bytes)?;
        ensure!(
            bytes == info.bytes && hash == info.sha256,
            "peer bundle checksum mismatch"
        );
        file.sync_all()?;
        rustix::fs::renameat(
            &self.bundles_dir,
            name,
            &self.bundles_dir,
            format!("{}.tar.gz", info.id),
        )?;
        self.bundles_dir.sync_all()?;
        Ok(())
    }
    pub(crate) fn quota(&self) -> Result<()> {
        self.reserve_bundle_space(0)
    }
    fn reserve_bundle_space(&self, bytes: u64) -> Result<()> {
        ensure!(
            bytes <= self.config.bundle_max_bytes,
            "incoming bundle exceeds cap"
        );
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let evicted = self.evict_bundles_to(&state, self.config.bundle_quota_bytes - bytes)?;
        self.mark_owner_evictions(&mut state, &evicted)
    }
    fn mark_owner_evictions(&self, state: &mut RegistryState, evicted: &[String]) -> Result<()> {
        let records = state
            .records
            .values()
            .filter(|s| {
                s.record.machine == self.owner
                    && s.record
                        .bundle
                        .as_ref()
                        .is_some_and(|info| info.available && evicted.contains(&info.id))
            })
            .cloned()
            .collect::<Vec<_>>();
        for mut stored in records {
            if let Some(info) = &mut stored.record.bundle {
                info.available = false;
            }
            self.commit(state, stored)?;
        }
        Ok(())
    }
    fn evict_bundles_to(&self, state: &RegistryState, quota: u64) -> Result<Vec<String>> {
        let times = state
            .records
            .values()
            .filter_map(|s| {
                Some((
                    s.record.bundle.as_ref()?.id.clone(),
                    s.record.archived_ms.unwrap_or(0),
                ))
            })
            .collect::<BTreeMap<_, _>>();
        let mut files = Vec::new();
        let mut total = 0_u64;
        let mut dir = Dir::read_from(&self.bundles_dir)?;
        while let Some(entry) = dir.read() {
            let entry = entry?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            let Some(id) = name.strip_suffix(".tar.gz") else {
                continue;
            };
            let file = open_at(&self.bundles_dir, name, OFlags::RDONLY, 0)?;
            let bytes = file.metadata()?.len();
            total = total.checked_add(bytes).context("bundle quota overflow")?;
            files.push((times.get(id).copied().unwrap_or(0), name.to_owned(), bytes));
        }
        files.sort();
        let mut evicted = Vec::new();
        for (_, name, bytes) in files {
            if total <= quota {
                break;
            }
            rustix::fs::unlinkat(&self.bundles_dir, &name, AtFlags::empty())?;
            evicted.push(name.trim_end_matches(".tar.gz").to_owned());
            total -= bytes;
        }
        self.bundles_dir.sync_all()?;
        Ok(evicted)
    }

    fn create_bundle(&self, stored: &StoredRecord) -> Result<BundleInfo> {
        let (temporary, file) = self.incoming_file()?;
        let result = self
            .write_bundle(stored, file)
            .and_then(|(mut file, native_log)| {
                file.sync_all()?;
                file.seek(SeekFrom::Start(0))?;
                let (sha256, bytes) = hash_reader(&mut file, self.config.bundle_max_bytes)?;
                let info = BundleInfo {
                    available: true,
                    id: sha256.clone(),
                    sha256,
                    bytes,
                    native_log,
                };
                rustix::fs::renameat(
                    &self.bundles_dir,
                    &temporary,
                    &self.bundles_dir,
                    format!("{}.tar.gz", info.id),
                )?;
                self.bundles_dir.sync_all()?;
                Ok(info)
            });
        if result.is_err() {
            self.discard_incoming(&temporary);
        }
        result
    }
    #[allow(clippy::too_many_lines)] // Native identity checks and bounded archive copy share descriptor ownership.
    fn write_bundle(&self, stored: &StoredRecord, file: File) -> Result<(File, bool)> {
        let mut native_files = Vec::new();
        if let Some(native) = &stored.native {
            let root = workspace::open_absolute_directory(&native.config_root)?;
            let relative = native.log_path.strip_prefix(&native.config_root)?;
            let log = open_relative(&root, relative)?;
            ensure!(log.metadata()?.is_file(), "native log is not a file");
            native_files.push((PathBuf::from("native").join(relative), log));
            if stored.record.harness == "claude" {
                let sibling = relative.with_extension("");
                let parent = sibling.parent().context("missing native parent")?;
                let parent_fd = open_relative(&root, parent)?;
                let name = sibling.file_name().context("missing native sibling")?;
                match rustix::fs::statat(&parent_fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(_) => {
                        let child = open_relative(&root, &sibling)?;
                        ensure!(
                            child.metadata()?.is_dir(),
                            "Claude sibling is not a directory"
                        );
                        let relative = PathBuf::from("native").join(sibling);
                        native_files.push((relative.clone(), child.try_clone()?));
                        collect_tree(&child, &relative, 0, &mut native_files)?;
                    }
                    Err(rustix::io::Errno::NOENT) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        let mut entries = Vec::new();
        let mut total = 0_u64;
        for (path, file) in &mut native_files {
            let directory = file.metadata()?.is_dir();
            let (checksum, bytes) = if directory {
                (format!("{:x}", Sha256::digest([])), 0)
            } else {
                hash_reader(file, self.config.bundle_max_bytes.saturating_sub(total))?
            };
            total = total.checked_add(bytes).context("bundle size overflow")?;
            if !directory {
                file.seek(SeekFrom::Start(0))?;
            }
            entries.push(ManifestFile {
                path: path.to_string_lossy().into_owned(),
                bytes,
                sha256: checksum,
                directory,
            });
        }
        let manifest = serde_json::to_vec(&BundleManifest {
            schema: "atmux.session.archive/v1".to_owned(),
            session: stored.clone(),
            files: entries.clone(),
        })?;
        ensure!(
            total + manifest.len() as u64 <= self.config.bundle_max_bytes,
            "bundle exceeds uncompressed size cap"
        );
        let writer = CappedWriter {
            inner: file,
            written: 0,
            limit: self.config.bundle_max_bytes,
        };
        let mut tar = tar::Builder::new(GzEncoder::new(writer, Compression::fast()));
        append_file(
            &mut tar,
            Path::new("manifest.json"),
            manifest.len() as u64,
            manifest.as_slice(),
        )?;
        for ((path, file), entry) in native_files.iter_mut().zip(&entries) {
            if entry.directory {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_mode(0o700);
                header.set_cksum();
                tar.append_data(&mut header, path, std::io::empty())?;
                continue;
            }
            let before = file.metadata()?;
            ensure!(
                before.len() == entry.bytes,
                "native log changed during archive"
            );
            let mut checked = HashingReader {
                inner: file.take(entry.bytes),
                digest: Sha256::new(),
            };
            append_file(&mut tar, path, entry.bytes, &mut checked)?;
            ensure!(
                format!("{:x}", checked.digest.finalize()) == entry.sha256,
                "native file changed during archive"
            );
            let after = file.metadata()?;
            ensure!(
                before.len() == after.len()
                    && before.mtime() == after.mtime()
                    && before.mtime_nsec() == after.mtime_nsec(),
                "native file changed during archive"
            );
        }
        let writer = tar.into_inner()?.finish()?;
        Ok((writer.inner, !native_files.is_empty()))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default)]
    pub directory: bool,
}

// A4 extends this same store and archive contract. Native identities remain
// peer-only; no resume cache or second registry is created.
impl Registry {
    /// Writes a bounded intent into the authoritative registry's mailbox. The
    /// single owner consumes it before observing panes, including after restart.
    /// This allows a TUI to record intent without opening a second registry writer.
    pub(crate) fn close_intents(config: &RegistryConfig, sessions: &[Session]) -> Result<()> {
        if !config.enabled {
            return Ok(());
        }
        let root = if let Some(path) = &config.directory {
            path.clone()
        } else {
            let dirs = directories::ProjectDirs::from("dev", "ryanmurf", "atmux")
                .context("state directory unavailable")?;
            dirs.state_dir()
                .unwrap_or_else(|| dirs.data_local_dir())
                .join("registry")
        };
        private_directory(&root)?;
        private_directory(&root.join("close-intents"))?;
        let dir = workspace::open_absolute_directory(&root.join("close-intents"))?;
        for session in sessions.iter().take(config.max_owner_records) {
            if let Some(key) = &session.session_key
                && tmux::valid_session_key(key)
            {
                let bytes =
                    serde_json::to_vec(&(session.pane_identity.clone(), session.agent_pid))?;
                ensure!(bytes.len() <= 8192, "close intent exceeds cap");
                atomic_write(&dir, &format!("{key}.json"), &bytes)?;
            }
        }
        Ok(())
    }

    fn consume_close_intents(&self, state: &mut RegistryState, at_ms: u64) -> Result<()> {
        let mut directory = Dir::read_from(&self.intents_dir)?;
        let mut count = 0;
        while let Some(entry) = directory.read() {
            let entry = entry?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            let Some(key) = name.strip_suffix(".json") else {
                continue;
            };
            ensure!(
                tmux::valid_session_key(key),
                "invalid close intent filename"
            );
            count += 1;
            ensure!(
                count <= self.config.max_owner_records,
                "close intent count exceeds cap"
            );
            let (instance, pid): (String, Option<u32>) =
                serde_json::from_slice(&read_at(&self.intents_dir, name, 8192)?)?;
            if let Some(mut record) = state
                .records
                .get(key)
                .filter(|record| {
                    record.record.machine == self.owner
                        && record.instance_id == instance
                        && record.agent_pid == pid
                })
                .cloned()
            {
                record.desired_running = Some(false);
                record.close_reason = Some("user".into());
                record.record.state = SessionState::Closed;
                record.record.closed_ms = Some(at_ms);
                self.commit(state, record)?;
            }
            rustix::fs::unlinkat(&self.intents_dir, name, AtFlags::empty())?;
        }
        Ok(())
    }

    pub(crate) fn bundle_limit(&self) -> u64 {
        self.config.bundle_max_bytes
    }

    pub(crate) fn owner_boot_id(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .owner_boot_id
            .clone()
    }

    pub(crate) fn resume_transaction(&self) -> std::sync::MutexGuard<'_, ()> {
        self.resume_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn stored(&self, key: &str) -> Result<Option<StoredRecord>> {
        ensure!(tmux::valid_session_key(key), "invalid session key");
        Ok(self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .records
            .get(key)
            .cloned())
    }

    pub(crate) fn restore_keys(&self, machine: &str, boot: &str) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .records
            .values()
            .filter(|stored| {
                stored.record.machine == machine
                    && stored.desired_running == Some(true)
                    && stored.close_reason.as_deref() == Some("node_loss")
                    && stored.owner_boot_id.as_deref() != Some(boot)
            })
            .map(|stored| stored.record.session_key.clone())
            .take(512)
            .collect()
    }

    pub(crate) fn restore_desired(&self, key: &str) -> Result<bool> {
        Ok(self.stored(key)?.is_some_and(|stored| {
            stored.record.machine == self.owner
                && stored.desired_running == Some(true)
                && (stored.close_reason.as_deref() == Some("node_loss")
                    || stored.record.state == SessionState::Running)
        }))
    }

    pub(crate) fn bind_native(
        &self,
        key: &str,
        native: NativeIdentity,
        process_start: Option<String>,
    ) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stored = state.records.get(key).cloned().context("unknown session")?;
        ensure!(
            stored.record.machine == self.owner,
            "cannot bind another owner's native identity"
        );
        stored.native = Some(native);
        stored.process_start = process_start;
        self.commit(&mut state, stored)
    }

    pub(crate) fn capture_bundle(&self, key: &str) -> Result<(File, BundleInfo)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stored = state.records.get(key).cloned().context("unknown session")?;
        ensure!(
            stored.record.machine == self.owner,
            "cannot read a foreign native store"
        );
        let evicted = self.evict_bundles_to(
            &state,
            self.config.bundle_quota_bytes - self.config.bundle_max_bytes,
        )?;
        self.mark_owner_evictions(&mut state, &evicted)?;
        let info = self.create_bundle(&stored)?;
        stored.record.bundle = Some(info);
        self.commit(&mut state, stored)?;
        drop(state);
        self.bundle_file(key)
    }

    pub(crate) fn record_close(&self, keys: &[String], at_ms: u64) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut events = Vec::new();
        for key in keys {
            let Some(mut stored) = state.records.get(key).cloned() else {
                continue;
            };
            ensure!(
                stored.record.machine == self.owner,
                "cannot close a foreign registry record"
            );
            stored.desired_running = Some(false);
            stored.close_reason = Some("user".to_owned());
            stored.record.state = SessionState::Closed;
            stored.record.closed_ms = Some(at_ms);
            self.commit(&mut state, stored.clone())?;
            events.push(LifecycleEvent {
                event_type: "session.closed",
                record: stored.record,
            });
        }
        let sink = state.sink.clone();
        drop(state);
        if let Some(sink) = sink {
            for event in events {
                sink(event);
            }
        }
        Ok(())
    }

    pub(crate) fn commit_resume(
        &self,
        mut stored: StoredRecord,
        session: &Session,
        native: NativeIdentity,
        generation: u64,
        at_ms: u64,
    ) -> Result<StoredRecord> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stored.record.machine.clone_from(&self.owner);
        stored.record.name.clone_from(&session.name);
        stored.record.cwd = session.path.to_string_lossy().into_owned();
        stored.record.last_seen_ms = at_ms;
        session
            .status
            .label()
            .clone_into(&mut stored.record.last_status);
        stored.record.state = SessionState::Running;
        stored.record.closed_ms = None;
        stored.record.archived_ms = None;
        stored.record.archive_error = None;
        stored.record.bundle = None;
        stored.pane_id.clone_from(&session.pane_id);
        stored.instance_id.clone_from(&session.pane_identity);
        stored.agent_pid = session.agent_pid;
        stored.process_start = session
            .agent_pid
            .and_then(crate::control::native_process_start_stamp);
        stored.native = Some(native);
        stored.owner_boot_id = Some(state.owner_boot_id.clone());
        stored.server_id.clone_from(&state.owner_server);
        stored.desired_running = Some(true);
        stored.close_reason = None;
        stored.resume_generation = generation;
        self.commit(&mut state, stored)?;
        Ok(state.records[session
            .session_key
            .as_deref()
            .context("resumed pane has no key")?]
        .clone())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BundleManifest {
    pub schema: String,
    pub session: StoredRecord,
    pub files: Vec<ManifestFile>,
}

fn append_file<W: Write, R: Read>(
    tar: &mut tar::Builder<W>,
    path: &Path,
    size: u64,
    reader: R,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(&mut header, path, reader)?;
    Ok(())
}
struct HashingReader<R> {
    inner: R,
    digest: Sha256,
}
impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(bytes)?;
        self.digest.update(&bytes[..n]);
        Ok(n)
    }
}
struct CappedWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
}
impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.limit.saturating_sub(self.written) {
            return Err(std::io::Error::other("bundle exceeds compressed size cap"));
        }
        let n = self.inner.write(bytes)?;
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
fn collect_tree(
    dir: &File,
    relative: &Path,
    depth: usize,
    files: &mut Vec<(PathBuf, File)>,
) -> Result<()> {
    ensure!(depth < MAX_DEPTH, "native directory too deep");
    let mut entries = Dir::read_from(dir)?;
    let mut count = 0;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        count += 1;
        ensure!(
            count <= MAX_FILES && files.len() < MAX_FILES,
            "too many native files"
        );
        let name = name.to_str().context("non-UTF8 native filename")?;
        ensure!(name.len() <= 255, "native filename too long");
        let file = open_at(dir, name, OFlags::RDONLY, 0)?;
        if file.metadata()?.is_dir() {
            files.push((relative.join(name), file.try_clone()?));
            collect_tree(&file, &relative.join(name), depth + 1, files)?;
        } else {
            files.push((relative.join(name), file));
        }
    }
    Ok(())
}
fn open_relative(root: &File, path: &Path) -> Result<File> {
    let mut file = root.try_clone()?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            bail!("unsafe relative native path")
        };
        file = open_at(&file, name, OFlags::RDONLY, 0)?;
    }
    Ok(file)
}
fn open_at(dir: &File, name: impl rustix::path::Arg, flags: OFlags, mode: u32) -> Result<File> {
    let file = File::from(rustix::fs::openat(
        dir,
        name,
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY,
        Mode::from_raw_mode(checked_mode(mode)?),
    )?);
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_dir() || metadata.nlink() == 1,
        "refusing hardlinked native/registry file"
    );
    ensure!(
        metadata.is_file() || metadata.is_dir(),
        "refusing special registry/native file"
    );
    Ok(file)
}
fn checked_mode<T: TryFrom<u32>>(mode: u32) -> Result<T> {
    T::try_from(mode).map_err(|_| anyhow::anyhow!("invalid file mode"))
}
fn read_at(dir: &File, name: &str, limit: u64) -> Result<Vec<u8>> {
    let file = open_at(dir, name, OFlags::RDONLY, 0)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit,
        "oversized/nonregular registry file"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "registry read exceeded bound");
    Ok(bytes)
}
fn atomic_write(dir: &File, name: &str, bytes: &[u8]) -> Result<()> {
    let temporary = format!("{}.tmp", tmux::new_session_key()?);
    let result = (|| {
        let mut file = open_at(
            dir,
            &temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            0o600,
        )?;
        file.write_all(bytes)?;
        file.sync_all()?;
        rustix::fs::renameat(dir, &temporary, dir, name)?;
        dir.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(dir, &temporary, AtFlags::empty());
    }
    result
}
fn private_directory(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "registry directory must be absolute");
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW;
    let mut current = workspace::open_absolute_directory(Path::new("/"))?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                match rustix::fs::mkdirat(&current, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                current = open_at(&current, name, flags, 0)?;
            }
            _ => bail!("unsafe registry directory component"),
        }
    }
    let metadata = current.metadata()?;
    ensure!(
        metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.permissions().mode().trailing_zeros() >= 6,
        "registry directory must be owned and private (0700)"
    );
    Ok(())
}

fn hash_reader(reader: &mut impl Read, limit: u64) -> Result<(String, u64)> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut bytes = 0_u64;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        ensure!(bytes <= limit, "bundle read exceeds size cap");
        digest.update(&buffer[..n]);
    }
    Ok((format!("{:x}", digest.finalize()), bytes))
}
fn harness(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Other => "other",
    }
}
fn contains(value: &str, needle: &str) -> bool {
    value.to_lowercase().contains(&needle.to_lowercase())
}
fn validate_search(query: &SessionsSearch) -> Result<()> {
    for value in [&query.machine, &query.project, &query.text] {
        ensure!(
            value.as_ref().is_none_or(|v| v.len() <= 2048),
            "oversized session filter"
        );
    }
    if let Some(cursor) = &query.cursor {
        ensure!(tmux::valid_session_key(cursor), "invalid session cursor");
    }
    Ok(())
}

/// Strips URL userinfo, query/fragment credentials and SSH usernames. Unknown
/// protocols/local paths are omitted rather than guessing how to redact them.
#[must_use]
pub fn credential_free_remote(value: &str) -> Option<String> {
    let value = value.trim().split(['?', '#']).next()?;
    if value.contains(['\n', '\r', '\0']) {
        return None;
    }
    if let Some((scheme, rest)) = value.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        if host.is_empty() || path.is_empty() {
            return None;
        }
        Some(format!("{scheme}://{host}/{path}"))
    } else {
        let (host, path) = value.rsplit('@').next()?.split_once(':')?;
        if host.contains('/') || host.is_empty() || path.is_empty() {
            return None;
        }
        Some(format!("ssh://{host}/{path}"))
    }
}
fn git_position(cwd: &Path) -> ProjectPosition {
    let Some(root) = git_text(cwd, &["rev-parse", "--show-toplevel"]) else {
        return ProjectPosition::default();
    };
    ProjectPosition {
        root,
        remote: git_text(cwd, &["config", "--get", "remote.origin.url"])
            .and_then(|v| credential_free_remote(&v)),
        branch: git_text(cwd, &["symbolic-ref", "--short", "HEAD"]),
        head: git_text(cwd, &["rev-parse", "HEAD"]),
        dirty: git_text(cwd, &["status", "--porcelain", "--untracked-files=normal"])
            .map(|v| !v.is_empty()),
    }
}
fn git_text(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false", "-c", "gc.auto=0"])
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut pipe = child.stdout.take()?;
    // Drain in parallel with the deadline so a full pipe cannot deadlock.
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        while let Ok(n) = pipe.read(&mut buffer) {
            if n == 0 {
                break;
            }
            if bytes.len() < 8192 {
                bytes.extend_from_slice(&buffer[..n.min(8192 - bytes.len())]);
            }
        }
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let bytes = reader.join().ok()?;
    success.then(|| String::from_utf8_lossy(&bytes).trim().to_owned())
}

/// One registry pull plus streaming checksum-verified bundle transfers.
/// Cursor commits only after the whole page is durable; failed transfers retry.
/// # Errors
/// Returns bounded transport, identity, checksum, or storage errors.
pub async fn pull_once(
    registry: &Arc<Registry>,
    remote: &crate::remote::RemoteMachine,
    after: Option<&str>,
) -> Result<String> {
    let path = after.map_or_else(
        || "/api/v1/registry?wait_ms=10000".to_owned(),
        |c| {
            format!(
                "/api/v1/registry?after={}&wait_ms=10000",
                crate::remote::encode_segment(c)
            )
        },
    );
    let page: RegistryPage = remote.get_json(&path).await?;
    let worker = Arc::clone(registry);
    let machine = remote.id.clone();
    let records = page.clone();
    tokio::task::spawn_blocking(move || worker.import_page(&machine, &records)).await??;
    let _permit = registry.archive_gate.acquire().await?;
    let worker = Arc::clone(registry);
    let pending_page = page.clone();
    let pending =
        tokio::task::spawn_blocking(move || worker.missing_bundles(&pending_page)).await?;
    for (key, info) in pending {
        let worker = Arc::clone(registry);
        let bytes = info.bytes;
        tokio::task::spawn_blocking(move || worker.reserve_bundle_space(bytes)).await??;
        let (temporary, file) = registry.incoming_file()?;
        let staged = IncomingCleanup {
            registry: Arc::clone(registry),
            name: temporary.clone(),
        };
        let path = format!("/api/v1/registry/{key}/bundle");
        if let Err(error) = remote
            .download_registry_bundle(&path, file, info.bytes)
            .await
        {
            if crate::remote::rejected_status(&error) == Some(404) {
                continue;
            }
            return Err(error);
        }
        let worker = Arc::clone(registry);
        let temp = temporary.clone();
        tokio::task::spawn_blocking(move || worker.finish_incoming(&temp, &info)).await??;
        drop(staged);
    }
    let worker = Arc::clone(registry);
    tokio::task::spawn_blocking(move || worker.quota()).await??;
    Ok(page.cursor)
}

struct IncomingCleanup {
    registry: Arc<Registry>,
    name: String,
}
impl Drop for IncomingCleanup {
    fn drop(&mut self) {
        self.registry.discard_incoming(&self.name);
    }
}

pub(crate) fn spawn_pull(
    registry: Arc<Registry>,
    remote: Arc<crate::remote::RemoteMachine>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        let mut cursor = None;
        loop {
            match pull_once(&registry, &remote, cursor.as_deref()).await {
                Ok(next) => {
                    cursor = Some(next);
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => {
                    eprintln!("atmux registry pull from {} failed: {error:#}", remote.id);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    })
    .abort_handle()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::fs;
    use std::os::unix::fs::symlink;

    struct Fixture {
        directory: PathBuf,
        config: RegistryConfig,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "atmux-registry-{}",
                tmux::new_session_key().unwrap()
            ));
            fs::create_dir(&directory).unwrap();
            let directory = directory.canonicalize().unwrap();
            let config = RegistryConfig {
                enabled: true,
                directory: Some(directory.join("store")),
                ..RegistryConfig::default()
            };
            Self { directory, config }
        }
        fn registry(&self) -> Arc<Registry> {
            Registry::open(&self.config, "owner").unwrap().unwrap()
        }
        fn native(&self, harness: &str) -> NativeIdentity {
            let root = self.directory.join(format!(".{harness}"));
            let id = "0199a5b7-5560-7abc-8def-0123456789ab";
            let relative = if harness == "claude" {
                format!("projects/-work/{id}.jsonl")
            } else {
                format!("sessions/2026/09/30/rollout-2026-09-30-{id}.jsonl")
            };
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"{\"type\":\"user\",\"message\":\"fixture\"}\n").unwrap();
            NativeIdentity {
                config_root: root,
                session_id: id.to_owned(),
                log_path: path,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
    fn session() -> Session {
        let mut session = crate::control::test_session("registry-agent", "%1", "ready");
        session.session_key = Some(tmux::new_session_key().unwrap());
        session.path = PathBuf::from("/nonexistent/atmux-a3-test");
        session.agent = AgentKind::Claude;
        session
    }
    fn seed(registry: &Registry, native: NativeIdentity, harness: &str) -> String {
        let key = tmux::new_session_key().unwrap();
        let record = SessionRecord {
            session_key: key.clone(),
            machine: "owner".to_owned(),
            name: "fixture".to_owned(),
            cwd: "/nonexistent/atmux-a3-test".to_owned(),
            harness: harness.to_owned(),
            created_ms: 1000,
            last_seen_ms: 1000,
            last_active_ms: 1000,
            ..SessionRecord::default()
        };
        registry
            .commit(
                &mut registry.state.lock().unwrap(),
                StoredRecord {
                    record,
                    native: Some(native),
                    ..StoredRecord::default()
                },
            )
            .unwrap();
        key
    }
    fn entries(registry: &Registry, key: &str) -> BTreeMap<String, Vec<u8>> {
        let (file, _) = registry.bundle_file(key).unwrap();
        let mut archive = tar::Archive::new(GzDecoder::new(file));
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let name = entry.path().unwrap().to_string_lossy().into_owned();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                (name, bytes)
            })
            .collect()
    }

    #[test]
    fn optional_defaults_and_validation_are_fail_closed() {
        assert!(
            Registry::open(&RegistryConfig::default(), "owner")
                .unwrap()
                .is_none()
        );
        let config = RegistryConfig {
            bundle_max_bytes: 0,
            ..RegistryConfig::default()
        };
        assert!(config.validate().is_err());
        let mut record: SessionRecord = serde_json::from_str("{}").unwrap();
        assert_eq!(record.state, SessionState::Running);
        record.description = Some("ü".repeat(4096));
        record.bound();
        assert_eq!(record.description.unwrap().len(), 2048);
    }

    #[test]
    fn scan_changes_checkpoint_only_changes_and_preserve_identity_on_exit() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let mut session = session();
        registry.observe(&[session.clone()], 1000).unwrap();
        let key = session.session_key.clone().unwrap();
        let revision = *registry.changed.borrow();
        registry.observe(&[session.clone()], 1001).unwrap();
        assert_eq!(*registry.changed.borrow(), revision);
        session.description = Some("Current task".to_owned());
        session.status = AgentStatus::Waiting;
        registry.observe(&[session.clone()], 2000).unwrap();
        let record = registry.get(&key).unwrap().unwrap();
        assert_eq!(record.needs_input_reason.as_deref(), Some("idle_prompt"));
        assert_eq!(record.description_source.as_deref(), Some("user"));
        assert_eq!(record.state, SessionState::Running);
        session.agent = AgentKind::Other;
        session.agent_pid = None;
        registry.observe(&[session.clone()], 3000).unwrap();
        let record = registry.get(&key).unwrap().unwrap();
        assert_eq!(record.state, SessionState::Exited);
        assert_eq!(record.harness, "claude");
        registry.observe(&[], 4000).unwrap();
        assert_eq!(
            registry.get(&key).unwrap().unwrap().state,
            SessionState::Archived
        );
        assert_eq!(registry.get(&key).unwrap().unwrap().closed_ms, Some(4000));
    }

    #[tokio::test]
    async fn atomic_recovery_ignores_torn_temporary_and_resets_peer_epoch() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let session = session();
        registry.observe(&[session], 1000).unwrap();
        let page = registry.changes(None, 0).await.unwrap();
        assert!(page.reset);
        assert_eq!(page.records.len(), 1);
        assert!(Registry::open(&fixture.config, "owner").is_err());
        fs::write(
            fixture
                .config
                .directory
                .as_ref()
                .unwrap()
                .join("records/torn.tmp"),
            b"{\"record\":",
        )
        .unwrap();
        drop(registry);
        let recovered = fixture.registry();
        let replay = recovered.changes(Some(&page.cursor), 0).await.unwrap();
        assert!(replay.reset);
        assert_eq!(replay.records[0].record, page.records[0].record);
        assert!(recovered.changes(Some("bad"), 0).await.is_err());
        drop(recovered);
        fs::write(
            fixture.config.directory.as_ref().unwrap().join(format!(
                "records/{}.json",
                page.records[0].record.session_key
            )),
            b"{",
        )
        .unwrap();
        assert!(Registry::open(&fixture.config, "owner").is_err());
    }

    #[test]
    fn claude_bundle_preserves_siblings_empty_directories_and_checksums() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let native = fixture.native("claude");
        let sibling = native.log_path.with_extension("");
        fs::create_dir_all(sibling.join("subagents/empty")).unwrap();
        fs::write(sibling.join("subagents/agent.jsonl"), "child log").unwrap();
        let key = seed(&registry, native.clone(), "claude");
        registry.observe(&[], 2000).unwrap();
        let files = entries(&registry, &key);
        assert!(files.keys().any(|p| p.ends_with("subagents/empty")));
        assert!(files.keys().any(|p| p.ends_with("subagents/agent.jsonl")));
        let manifest: BundleManifest = serde_json::from_slice(&files["manifest.json"]).unwrap();
        assert_eq!(manifest.session.record.state, SessionState::Archived);
        assert_eq!(manifest.session.native, Some(native));
        for entry in manifest.files {
            assert_eq!(
                entry.sha256,
                format!("{:x}", Sha256::digest(&files[&entry.path]))
            );
        }
        let (_, info) = registry.bundle_file(&key).unwrap();
        fs::write(
            fixture
                .config
                .directory
                .as_ref()
                .unwrap()
                .join(format!("bundles/{}.tar.gz", info.id)),
            "corrupt",
        )
        .unwrap();
        assert!(registry.bundle_file(&key).is_err());
    }

    #[test]
    fn codex_rollout_and_record_only_archives_are_supported() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let native = fixture.native("codex");
        let key = seed(&registry, native, "codex");
        registry.observe(&[], 2000).unwrap();
        let files = entries(&registry, &key);
        assert_eq!(files.len(), 2);
        assert!(files.keys().any(|p| p.contains("rollout-")));
        let session = session();
        let key = session.session_key.clone().unwrap();
        registry.observe(&[session], 3000).unwrap();
        registry.observe(&[], 4000).unwrap();
        assert!(
            !registry
                .get(&key)
                .unwrap()
                .unwrap()
                .bundle
                .unwrap()
                .native_log
        );
        assert_eq!(entries(&registry, &key).len(), 1);
    }

    #[test]
    fn bundle_limits_and_symlinks_fail_closed_with_retryable_closed_record() {
        for variant in [
            "large",
            "file",
            "hardlink",
            "ancestor",
            "sibling",
            "descendant",
        ] {
            let mut fixture = Fixture::new();
            fixture.config.bundle_max_bytes = 64 * 1024;
            let registry = fixture.registry();
            let native = fixture.native("claude");
            let outside = fixture.directory.join("outside");
            fs::write(&outside, "private").unwrap();
            match variant {
                "large" => fs::write(&native.log_path, vec![b'x'; 65537]).unwrap(),
                "file" => {
                    fs::remove_file(&native.log_path).unwrap();
                    symlink(&outside, &native.log_path).unwrap();
                }
                "hardlink" => {
                    fs::remove_file(&native.log_path).unwrap();
                    fs::hard_link(&outside, &native.log_path).unwrap();
                }
                "ancestor" => {
                    let parent = native.log_path.parent().unwrap();
                    fs::rename(parent, parent.with_extension("real")).unwrap();
                    symlink(parent.with_extension("real"), parent).unwrap();
                }
                "sibling" => symlink(&outside, native.log_path.with_extension("")).unwrap(),
                _ => {
                    let sibling = native.log_path.with_extension("");
                    fs::create_dir(&sibling).unwrap();
                    symlink(&outside, sibling.join("escape")).unwrap();
                }
            }
            let key = seed(&registry, native, "claude");
            registry.observe(&[], 2000).unwrap();
            let record = registry.get(&key).unwrap().unwrap();
            assert_eq!(record.state, SessionState::Closed, "{variant}");
            assert!(record.bundle.is_none());
            assert_eq!(
                fs::read_dir(fixture.config.directory.as_ref().unwrap().join("bundles"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    #[test]
    fn events_are_emitted_once_after_each_durable_transition() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&observed);
        registry.set_event_sink(Arc::new(move |event| {
            sink.lock().unwrap().push(event.event_type);
        }));
        registry.observe(&[session()], 1000).unwrap();
        registry.observe(&[], 2000).unwrap();
        registry.observe(&[], 3000).unwrap();
        assert_eq!(
            *observed.lock().unwrap(),
            ["session.closed", "session.archived"]
        );
    }

    #[test]
    fn owner_retention_and_bundle_eviction_never_delete_central_records() {
        let mut fixture = Fixture::new();
        fixture.config.max_owner_records = 1;
        let registry = fixture.registry();
        let first = session();
        let first_key = first.session_key.clone().unwrap();
        registry.observe(&[first], 1000).unwrap();
        registry.observe(&[], 2000).unwrap();
        let second = session();
        registry
            .observe(std::slice::from_ref(&second), 3000)
            .unwrap();
        assert!(registry.get(&first_key).unwrap().is_none());
        registry.observe(&[], 4000).unwrap();
        registry.observe(&[], 91 * DAY_MS).unwrap();
        assert!(
            registry
                .get(second.session_key.as_ref().unwrap())
                .unwrap()
                .is_none()
        );
        let remote_key = tmux::new_session_key().unwrap();
        let remote = StoredRecord {
            record: SessionRecord {
                session_key: remote_key.clone(),
                machine: "peer".to_owned(),
                state: SessionState::Archived,
                ..SessionRecord::default()
            },
            ..StoredRecord::default()
        };
        registry
            .import_page(
                "peer",
                &RegistryPage {
                    cursor: String::new(),
                    reset: true,
                    more: false,
                    records: vec![remote],
                },
            )
            .unwrap();
        registry.observe(&[], 365 * DAY_MS).unwrap();
        assert!(registry.get(&remote_key).unwrap().is_some());
        assert!(
            registry
                .import_page(
                    "spoof",
                    &RegistryPage {
                        cursor: String::new(),
                        reset: false,
                        more: false,
                        records: vec![registry.state.lock().unwrap().records[&remote_key].clone()]
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn public_search_get_and_peer_import_redact_native_identity_and_remote_credentials() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let native = fixture.native("claude");
        let key = seed(&registry, native.clone(), "claude");
        let query = SessionsSearch {
            text: Some("FIXTURE".to_owned()),
            ..SessionsSearch::default()
        };
        let json = serde_json::to_string(&registry.search(&query).unwrap()).unwrap();
        let get = serde_json::to_string(&registry.get(&key).unwrap()).unwrap();
        for body in [json, get] {
            assert!(!body.contains(&native.session_id));
            assert!(!body.contains("config_root"));
            assert!(!body.contains("log_path"));
        }
        for (input, expected) in [
            (
                "https://user:password@github.com/org/repo?token=secret#fragment",
                "https://github.com/org/repo",
            ),
            (
                "git@github.com:org/repo.git",
                "ssh://github.com/org/repo.git",
            ),
        ] {
            assert_eq!(credential_free_remote(input).as_deref(), Some(expected));
        }
        assert!(credential_free_remote("file:///private/token").is_none());
    }

    #[tokio::test]
    async fn feed_pagination_long_poll_and_heartbeat_do_not_lose_changes() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let sessions = (0..20).map(|_| session()).collect::<Vec<_>>();
        registry.observe(&sessions, 1000).unwrap();
        let first = registry.changes(None, 0).await.unwrap();
        assert_eq!(first.records.len(), PAGE_SIZE);
        assert!(first.more);
        let second = registry.changes(Some(&first.cursor), 0).await.unwrap();
        assert_eq!(second.records.len(), 4);
        assert!(!second.more);
        let waiter = Arc::clone(&registry);
        let cursor = second.cursor.clone();
        let pending =
            tokio::spawn(async move { waiter.changes(Some(&cursor), 1000).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        registry.observe(&sessions, 32_000).unwrap();
        assert!(!pending.await.unwrap().records.is_empty());
    }
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // End-to-end fixture exercises authentication, redaction, streaming and replay.
    async fn fixture_owner_federates_records_and_checksum_verified_streamed_bundles() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt as _;
        let fixture = Fixture::new();
        let source = fixture.registry();
        let native = fixture.native("claude");
        let key = seed(&source, native.clone(), "claude");
        source.observe(&[], 2000).unwrap();
        let expected = source.get(&key).unwrap().unwrap();
        drop(source);
        let token_path = fixture.directory.join("node.token");
        fs::write(&token_path, "a3-fixture-node-token").unwrap();
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut config = crate::config::Config::default();
        config.node.id = "owner".to_owned();
        config.node.coordinator_only = true;
        config.profiles.clear();
        config.general.project_roots.clear();
        config.general.favorite_dirs.clear();
        config.general.switch_on_launch = false;
        config.node.token_file = Some(token_path.clone());
        config.registry = fixture.config.clone();
        let control = crate::control::ControlPlane::start(config).await.unwrap();
        let (_, shutdown) = watch::channel(false);
        let app = crate::web::api_router(control.clone(), Vec::new(), shutdown);
        for (header, value) in [
            ("origin", "http://browser.test"),
            ("sec-fetch-site", "same-origin"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/v1/registry?wait_ms=0")
                        .header("authorization", "Bearer a3-fixture-node-token")
                        .header(header, value)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let rejected = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/registry?wait_ms=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        for uri in [
            format!("/api/v1/session-history/{key}"),
            "/api/v1/session-history".to_owned(),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let body = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(!body.contains(&native.session_id));
            assert!(!body.contains("config_root"));
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let receiver_fixture = Fixture::new();
        let receiver = Registry::open(&receiver_fixture.config, "coordinator")
            .unwrap()
            .unwrap();
        let remote = crate::remote::RemoteMachine::from_config(&crate::config::MachineConfig {
            id: "owner".to_owned(),
            label: None,
            url: format!("http://{address}"),
            token_env: None,
            token_file: Some(token_path),
        })
        .unwrap();
        let cursor = pull_once(&receiver, &remote, None).await.unwrap();
        assert_eq!(receiver.get(&key).unwrap().unwrap(), expected);
        assert_eq!(
            entries(&receiver, &key),
            entries(&control.registry().unwrap(), &key)
        );
        // Cursor resets replay a page without rewriting unchanged records.
        let revision = *receiver.changed.borrow();
        pull_once(&receiver, &remote, None).await.unwrap();
        assert_eq!(*receiver.changed.borrow(), revision);
        let peer = control
            .registry()
            .unwrap()
            .changes(Some(&cursor), 0)
            .await
            .unwrap();
        assert!(peer.records.is_empty());
        // Corrupt copies are retried from the owner on replay.
        let info = expected.bundle.unwrap();
        fs::write(
            receiver_fixture
                .config
                .directory
                .as_ref()
                .unwrap()
                .join(format!("bundles/{}.tar.gz", info.id)),
            "tampered",
        )
        .unwrap();
        pull_once(&receiver, &remote, None).await.unwrap();
        assert!(receiver.bundle_file(&key).is_ok());
        fs::remove_file(
            receiver_fixture
                .config
                .directory
                .as_ref()
                .unwrap()
                .join(format!("bundles/{}.tar.gz", info.id)),
        )
        .unwrap();
        fs::remove_file(
            fixture
                .config
                .directory
                .as_ref()
                .unwrap()
                .join(format!("bundles/{}.tar.gz", info.id)),
        )
        .unwrap();
        let after_missing = pull_once(&receiver, &remote, None).await.unwrap();
        assert!(
            !receiver
                .get(&key)
                .unwrap()
                .unwrap()
                .bundle
                .unwrap()
                .available
        );
        let source = control.registry().unwrap();
        let second_key = seed(&source, fixture.native("codex"), "codex");
        source.observe(&[], 3000).unwrap();
        pull_once(&receiver, &remote, Some(&after_missing))
            .await
            .unwrap();
        assert!(receiver.bundle_file(&second_key).is_ok());
        server.abort();
    }

    #[test]
    fn quota_evicts_oldest_bundle_without_losing_searchable_records() {
        let mut fixture = Fixture::new();
        fixture.config.bundle_max_bytes = 64 * 1024;
        fixture.config.bundle_quota_bytes = 64 * 1024;
        let registry = fixture.registry();
        let mut keys = Vec::new();
        for at in [1000, 2000] {
            let native = fixture.native("codex");
            let key = seed(&registry, native, "codex");
            registry.observe(&[], at).unwrap();
            keys.push(key);
        }
        for key in &keys {
            let info = registry.get(key).unwrap().unwrap().bundle.unwrap();
            // Force the quota using ordinary files of bounded sizes.
            fs::write(
                fixture
                    .config
                    .directory
                    .as_ref()
                    .unwrap()
                    .join(format!("bundles/{}.tar.gz", info.id)),
                vec![0; 40 * 1024],
            )
            .unwrap();
        }
        registry.quota().unwrap();
        assert!(keys.iter().all(|key| registry.get(key).unwrap().is_some()));
        let first = registry.get(&keys[0]).unwrap().unwrap().bundle.unwrap();
        let last = registry.get(&keys[1]).unwrap().unwrap().bundle.unwrap();
        assert!(
            !fixture
                .config
                .directory
                .as_ref()
                .unwrap()
                .join(format!("bundles/{}.tar.gz", first.id))
                .exists()
        );
        assert!(
            fixture
                .config
                .directory
                .as_ref()
                .unwrap()
                .join(format!("bundles/{}.tar.gz", last.id))
                .exists()
        );
    }
    #[test]
    fn storage_creation_rejects_symlink_ancestors_before_writing() {
        let mut fixture = Fixture::new();
        let outside = fixture.directory.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, fixture.directory.join("link")).unwrap();
        fixture.config.directory = Some(fixture.directory.join("link/uncreated/registry"));
        assert!(Registry::open(&fixture.config, "owner").is_err());
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn digest_reference_and_precise_input_reason_survive_waiting_scans() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let mut session = session();
        session.status = AgentStatus::Waiting;
        let key = session.session_key.clone().unwrap();
        registry
            .observe(std::slice::from_ref(&session), 1000)
            .unwrap();
        registry
            .set_digest_reference(&key, Some("digest-v3".to_owned()))
            .unwrap();
        registry
            .set_needs_input_reason(&key, Some("permission".to_owned()))
            .unwrap();
        registry
            .observe(std::slice::from_ref(&session), 2000)
            .unwrap();
        let record = registry.get(&key).unwrap().unwrap();
        assert_eq!(record.needs_input_reason.as_deref(), Some("permission"));
        assert_eq!(record.digest_ref.as_deref(), Some("digest-v3"));
        assert!(
            registry
                .set_needs_input_reason(&key, Some("arbitrary".to_owned()))
                .is_err()
        );
        session.status = AgentStatus::Working;
        registry.observe(&[session], 3000).unwrap();
        assert!(
            registry
                .get(&key)
                .unwrap()
                .unwrap()
                .needs_input_reason
                .is_none()
        );
    }
    #[test]
    fn archive_captures_final_git_head_branch_dirty_state_and_redacted_remote() {
        let fixture = Fixture::new();
        let project = fixture.directory.join("project");
        fs::create_dir(&project).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&project)
                .args([
                    "-c",
                    "user.name=A3 fixture",
                    "-c",
                    "user.email=a3@example.test",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["init", "-q", "-b", "a3-fixture"]);
        fs::write(project.join("tracked.txt"), "original").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "-q", "-m", "fixture"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://user:password@example.test/org/repo?token=secret",
        ]);
        let head = git(&["rev-parse", "HEAD"]);
        let registry = fixture.registry();
        let mut session = session();
        session.path = project.clone();
        let key = session.session_key.clone().unwrap();
        registry.observe(&[session], 1000).unwrap();
        assert_eq!(
            registry.get(&key).unwrap().unwrap().project.dirty,
            Some(false)
        );
        fs::write(project.join("tracked.txt"), "changed").unwrap();
        registry.observe(&[], 2000).unwrap();
        let archived = registry.get(&key).unwrap().unwrap();
        assert_eq!(archived.project.dirty, Some(true));
        assert_eq!(archived.project.branch.as_deref(), Some("a3-fixture"));
        assert_eq!(archived.project.head.as_deref(), Some(head.as_str()));
        assert_eq!(
            archived.project.remote.as_deref(),
            Some("https://example.test/org/repo")
        );
        let manifest: BundleManifest =
            serde_json::from_slice(&entries(&registry, &key)["manifest.json"]).unwrap();
        assert_eq!(manifest.session.record.project, archived.project);
    }
}
