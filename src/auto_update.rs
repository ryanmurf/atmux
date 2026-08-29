//! Owner-local native Claude/Codex CLI maintenance primitives.
//!
//! The control plane owns pane selection and tmux mutation. This module owns
//! the cross-process lock, durable update generations, executable identity,
//! and the two fixed vendor update protocols. No request value can become a
//! program, URL, argument, or environment assignment here.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    future::Future,
    io::{Read as _, Write as _},
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use directories::ProjectDirs;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::process::Command;

const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 256 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const CODEX_INSTALL_URL: &str = "https://chatgpt.com/codex/install.sh";
pub(crate) const PENDING_OPTION: &str = "@atmux_cli_update_pending";
const MARKER_VERSION: &str = "cu2";
static TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// Bounded owner-local CLI maintenance policy. Disabled is the safe default.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MaintenanceConfig {
    pub enabled: bool,
    pub interval_minutes: u64,
    pub update_timeout_seconds: u64,
    pub relaunch_limit: usize,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 30,
            update_timeout_seconds: 180,
            relaunch_limit: 4,
        }
    }
}

impl MaintenanceConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(1..=7 * 24 * 60).contains(&self.interval_minutes) {
            bail!("[maintenance].interval_minutes must be between 1 and 10080");
        }
        if !(30..=15 * 60).contains(&self.update_timeout_seconds) {
            bail!("[maintenance].update_timeout_seconds must be between 30 and 900");
        }
        if !(1..=32).contains(&self.relaunch_limit) {
            bail!("[maintenance].relaunch_limit must be between 1 and 32");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Harness {
    Claude,
    Codex,
}

impl Harness {
    pub(crate) const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecutableIdentity {
    pub(crate) canonical_path: PathBuf,
    pub(crate) version: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) modified_ns: u128,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct HarnessState {
    pub(crate) generation: u64,
    pub(crate) before: Option<ExecutableIdentity>,
    pub(crate) after: Option<ExecutableIdentity>,
    pub(crate) last_error: Option<String>,
    /// Last launcher identity fully reconciled into an update generation.
    pub(crate) applied: Option<ExecutableIdentity>,
    /// Persisted before invoking a vendor updater. Its baseline makes an
    /// updater crash or a provider background update recoverable next poll.
    pub(crate) intent: Option<UpdateIntent>,
    /// Exact pre-update panes whose durable marker must be materialized. This
    /// remains until each pane is claimed, invalidated, or definitively stale.
    pub(crate) pending: Vec<PlannedPane>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlannedPane {
    pub(crate) pane_id: String,
    pub(crate) session_fingerprint: String,
    pub(crate) mutation_sequence: u64,
    pub(crate) profile: String,
    pub(crate) mode_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct UpdateIntent {
    pub(crate) baseline: ExecutableIdentity,
    pub(crate) observed_before: ExecutableIdentity,
    pub(crate) candidates: Vec<PlannedPane>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReconcileResult {
    NoChange,
    Changed { generation: u64 },
}

impl HarnessState {
    /// Persists an exact baseline and pane plan before a vendor process starts.
    /// The caller must store the surrounding `MaintenanceState` before `update()`.
    pub(crate) fn prepare_intent(
        &mut self,
        observed: ExecutableIdentity,
        candidates: Vec<PlannedPane>,
    ) {
        let baseline = self.applied.clone().unwrap_or_else(|| observed.clone());
        if self.applied.is_none() {
            self.applied = Some(baseline.clone());
        }
        self.before = Some(observed.clone());
        self.intent = Some(UpdateIntent {
            baseline,
            observed_before: observed,
            candidates,
        });
    }

    /// Reconciles either a normal updater completion or an interrupted intent
    /// observed by a restarted owner. Comparison is against the last durable
    /// applied baseline, not merely before/after one updater invocation.
    pub(crate) fn reconcile(
        &mut self,
        observed_final: ExecutableIdentity,
        update_error: Option<String>,
    ) -> ReconcileResult {
        let Some(intent) = self.intent.take() else {
            // A first observation outside an update establishes the durable
            // baseline without inventing a generation.
            self.applied = Some(observed_final.clone());
            self.after = Some(observed_final);
            self.last_error = update_error;
            return ReconcileResult::NoChange;
        };
        let did_change = changed(&intent.baseline, &observed_final);
        self.after = Some(observed_final.clone());
        self.applied = Some(observed_final);
        self.last_error = update_error;
        if did_change {
            self.generation = advance_generation(self.generation, true);
            self.pending = intent.candidates;
            ReconcileResult::Changed {
                generation: self.generation,
            }
        } else {
            ReconcileResult::NoChange
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MaintenanceState {
    version: u32,
    pub(crate) last_check_ms: Option<u64>,
    pub(crate) harnesses: BTreeMap<Harness, HarnessState>,
}

impl Default for MaintenanceState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            last_check_ms: None,
            harnesses: BTreeMap::new(),
        }
    }
}

impl MaintenanceState {
    pub(crate) fn harness_mut(&mut self, harness: Harness) -> &mut HarnessState {
        self.harnesses.entry(harness).or_default()
    }
}

pub(crate) type MaintenanceFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Injectable owner-local effects for one maintenance cycle. Production and
/// tests share the same orchestration, while tests never invoke a vendor CLI
/// or a tmux server.
pub(crate) trait MaintenanceRuntime: Sync {
    fn inspect(&self, harness: Harness) -> MaintenanceFuture<'_, Option<ExecutableIdentity>>;

    fn collect(
        &self,
        harness: Harness,
        before_launcher: PathBuf,
    ) -> MaintenanceFuture<'_, Vec<PlannedPane>>;

    fn update(
        &self,
        harness: Harness,
        before: ExecutableIdentity,
        timeout: Duration,
    ) -> MaintenanceFuture<'_, ExecutableIdentity>;

    fn persist(&self, state: &MaintenanceState) -> Result<()>;

    fn process_pending(
        &self,
        state: MaintenanceState,
        limit: usize,
    ) -> MaintenanceFuture<'_, (MaintenanceState, bool)>;
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CycleSettings {
    pub(crate) now_ms: u64,
    pub(crate) interval_ms: u64,
    pub(crate) update_timeout: Duration,
    pub(crate) relaunch_limit: usize,
}

/// Runs one complete durable maintenance transaction through injected effects.
/// Persist calls are deliberately placed before each vendor updater and after
/// every reconciliation so the same ordering is exercised in unit tests.
pub(crate) async fn run_maintenance_cycle(
    runtime: &impl MaintenanceRuntime,
    mut state: MaintenanceState,
    settings: CycleSettings,
) -> Result<(MaintenanceState, bool)> {
    let due = check_due(state.last_check_ms, settings.now_ms, settings.interval_ms);
    if state.last_check_ms.is_none() {
        establish_baselines(runtime, &mut state, settings.now_ms).await;
    } else if due {
        run_due_updates(runtime, &mut state, settings).await?;
    }
    recover_interrupted_intents(runtime, &mut state).await;
    let (state, resumed) = runtime
        .process_pending(state, settings.relaunch_limit)
        .await?;
    runtime.persist(&state)?;
    Ok((state, resumed))
}

async fn establish_baselines(
    runtime: &impl MaintenanceRuntime,
    state: &mut MaintenanceState,
    now_ms: u64,
) {
    state.last_check_ms = Some(now_ms);
    for harness in Harness::ALL {
        match runtime.inspect(harness).await {
            Ok(Some(identity)) => {
                state.harness_mut(harness).reconcile(identity, None);
            }
            Ok(None) => {
                state.harness_mut(harness).last_error =
                    Some(format!("{} native launcher is unavailable", harness.name()));
            }
            Err(error) => state.harness_mut(harness).last_error = Some(format!("{error:#}")),
        }
    }
}

async fn run_due_updates(
    runtime: &impl MaintenanceRuntime,
    state: &mut MaintenanceState,
    settings: CycleSettings,
) -> Result<()> {
    state.last_check_ms = Some(settings.now_ms);
    for harness in Harness::ALL {
        let before = match runtime.inspect(harness).await {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                state.harness_mut(harness).last_error =
                    Some(format!("{} native launcher is unavailable", harness.name()));
                continue;
            }
            Err(error) => {
                state.harness_mut(harness).last_error = Some(format!("{error:#}"));
                continue;
            }
        };
        let candidates = match runtime
            .collect(harness, before.canonical_path.clone())
            .await
        {
            Ok(candidates) => candidates,
            Err(error) => {
                state.harness_mut(harness).last_error = Some(format!("{error:#}"));
                continue;
            }
        };
        state
            .harness_mut(harness)
            .prepare_intent(before.clone(), candidates);
        runtime.persist(state)?;

        let result = runtime
            .update(harness, before, settings.update_timeout)
            .await;
        reconcile_update_result(runtime, state, harness, result).await;
        runtime.persist(state)?;
    }
    Ok(())
}

async fn reconcile_update_result(
    runtime: &impl MaintenanceRuntime,
    state: &mut MaintenanceState,
    harness: Harness,
    result: Result<ExecutableIdentity>,
) {
    let (final_identity, update_error) = match result {
        Ok(after) => (Some(after), None),
        Err(error) => (
            runtime.inspect(harness).await.ok().flatten(),
            Some(format!("{error:#}")),
        ),
    };
    if let Some(final_identity) = final_identity {
        state
            .harness_mut(harness)
            .reconcile(final_identity, update_error);
    } else {
        state.harness_mut(harness).last_error = update_error
            .or_else(|| Some("updated launcher identity is temporarily unavailable".to_owned()));
    }
}

async fn recover_interrupted_intents(
    runtime: &impl MaintenanceRuntime,
    state: &mut MaintenanceState,
) {
    for harness in Harness::ALL {
        if state
            .harnesses
            .get(&harness)
            .is_none_or(|harness_state| harness_state.intent.is_none())
        {
            continue;
        }
        match runtime.inspect(harness).await {
            Ok(Some(identity)) => {
                state.harness_mut(harness).reconcile(
                    identity,
                    Some("recovered an interrupted update intent".to_owned()),
                );
            }
            Ok(None) => {}
            Err(error) => state.harness_mut(harness).last_error = Some(format!("{error:#}")),
        }
    }
}

/// Held for a complete check/update/relaunch cycle. A second atmux process on
/// the same owner skips the cycle instead of becoming another scheduler.
pub(crate) struct OwnerLock {
    _file: File,
    state_path: PathBuf,
    temp_dir: PathBuf,
}

/// Cross-process pane mutation gate shared by every atmux owner process.
/// The in-memory gate remains useful for request ordering; this advisory lock
/// closes the old/new service overlap during restarts and rolling deploys.
pub(crate) struct PaneProcessLock {
    _file: File,
}

impl PaneProcessLock {
    pub(crate) fn acquire(pane_id: &str) -> Result<Self> {
        if !pane_id
            .strip_prefix('%')
            .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()))
        {
            bail!("pane mutation lock target is invalid");
        }
        let dirs = ProjectDirs::from("dev", "ryanmurf", "atmux")
            .context("could not determine atmux state directory")?;
        let root = dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .join("pane-locks");
        Self::acquire_in(&root, pane_id, false)?.context("pane mutation lock was unexpectedly busy")
    }

    #[cfg(test)]
    fn try_acquire_in(root: &Path, pane_id: &str) -> Result<Option<Self>> {
        Self::acquire_in(root, pane_id, true)
    }

    fn acquire_in(root: &Path, pane_id: &str, nonblocking: bool) -> Result<Option<Self>> {
        secure_owner_directory(root)?;
        let mut digest = Sha256::new();
        digest.update(pane_id.as_bytes());
        let path = root.join(format!("{:x}.lock", digest.finalize()));
        let file = secure_open(&path)?;
        let acquired = if nonblocking {
            match file.try_lock_exclusive() {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
                Err(error) => {
                    return Err(error).context("could not acquire the owner pane mutation lock");
                }
            }
        } else {
            file.lock_exclusive()
                .context("could not acquire the owner pane mutation lock")?;
            true
        };
        Ok(acquired.then_some(Self { _file: file }))
    }
}

impl OwnerLock {
    pub(crate) fn try_acquire() -> Result<Option<Self>> {
        let dirs = ProjectDirs::from("dev", "ryanmurf", "atmux")
            .context("could not determine atmux state directory")?;
        let root = dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .join("maintenance");
        Self::try_acquire_in(root)
    }

    fn try_acquire_in(root: PathBuf) -> Result<Option<Self>> {
        secure_owner_directory(&root)?;
        let lock_path = root.join("owner.lock");
        let file = secure_open(&lock_path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self {
                _file: file,
                state_path: root.join("state.json"),
                temp_dir: root,
            })),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error).context("could not lock owner CLI maintenance"),
        }
    }

    pub(crate) fn load(&self) -> Result<MaintenanceState> {
        if !self.state_path.exists() {
            return Ok(MaintenanceState::default());
        }
        reject_symlink_or_unowned(&self.state_path)?;
        let mut bytes = Vec::new();
        File::open(&self.state_path)?
            .take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            bail!("CLI maintenance state exceeds its size bound");
        }
        let state: MaintenanceState =
            serde_json::from_slice(&bytes).context("CLI maintenance state is not valid JSON")?;
        if state.version != STATE_VERSION {
            bail!("CLI maintenance state has an unsupported version");
        }
        Ok(state)
    }

    pub(crate) fn store(&self, state: &MaintenanceState) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            bail!("CLI maintenance state exceeds its size bound");
        }
        let temp = self.temp_path("state");
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, &self.state_path)?;
        File::open(&self.temp_dir)?.sync_all()?;
        Ok(())
    }

    fn temp_path(&self, label: &str) -> PathBuf {
        self.temp_dir.join(format!(
            ".{label}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingMarker {
    pub(crate) harness: Harness,
    pub(crate) generation: u64,
    pub(crate) session_fingerprint: String,
    pub(crate) phase: MarkerPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MarkerPhase {
    Ready,
    Claimed,
}

impl PendingMarker {
    pub(crate) fn encode(&self) -> String {
        format!(
            "{MARKER_VERSION}:{}:{}:{}:{}",
            self.harness.name(),
            self.generation,
            self.session_fingerprint,
            match self.phase {
                MarkerPhase::Ready => "ready",
                MarkerPhase::Claimed => "claimed",
            }
        )
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split(':');
        if parts.next()? != MARKER_VERSION {
            return None;
        }
        let harness = match parts.next()? {
            "claude" => Harness::Claude,
            "codex" => Harness::Codex,
            _ => return None,
        };
        let generation = parts.next()?.parse().ok()?;
        let session_fingerprint = parts.next()?.to_owned();
        let phase = match parts.next()? {
            "ready" => MarkerPhase::Ready,
            "claimed" => MarkerPhase::Claimed,
            _ => return None,
        };
        if parts.next().is_some()
            || session_fingerprint.len() != 64
            || !session_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return None;
        }
        Some(Self {
            harness,
            generation,
            session_fingerprint,
            phase,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaneObservation {
    pub(crate) exists: bool,
    pub(crate) identity_definitively_stale: bool,
    pub(crate) session_fingerprint: Option<String>,
    pub(crate) mutation_sequence: Option<u64>,
    pub(crate) exact_idle: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneAction {
    MaterializeReady,
    Defer,
    ClaimBeforeRespawn,
    Forget,
    AlreadyClaimed,
}

/// Shared crash-safe pane protocol used by production and injected tests.
/// Transient missing status/model/idle proof defers; only disappearance,
/// definite identity change, or mutation-sequence advance forgets a plan.
pub(crate) fn pane_action(
    planned: &PlannedPane,
    marker: Option<&PendingMarker>,
    observation: &PaneObservation,
    harness: Harness,
    generation: u64,
) -> PaneAction {
    if marker.is_some_and(|marker| {
        marker.harness == harness
            && marker.generation == generation
            && marker.session_fingerprint == planned.session_fingerprint
            && marker.phase == MarkerPhase::Claimed
    }) {
        // This includes a crash after the claim but before state cleanup in
        // this exact generation. Never risk a second destructive respawn.
        return PaneAction::AlreadyClaimed;
    }
    if !observation.exists
        || observation.identity_definitively_stale
        || observation
            .mutation_sequence
            .is_some_and(|sequence| sequence != planned.mutation_sequence)
        || observation
            .session_fingerprint
            .as_deref()
            .is_some_and(|fingerprint| fingerprint != planned.session_fingerprint)
    {
        return PaneAction::Forget;
    }
    let expected = marker.is_some_and(|marker| {
        marker.harness == harness
            && marker.generation == generation
            && marker.session_fingerprint == planned.session_fingerprint
    });
    if !expected {
        return PaneAction::MaterializeReady;
    }
    match marker.map(|marker| marker.phase) {
        Some(MarkerPhase::Claimed) => PaneAction::AlreadyClaimed,
        Some(MarkerPhase::Ready)
            if observation.exact_idle
                && observation.session_fingerprint.is_some()
                && observation.mutation_sequence.is_some() =>
        {
            PaneAction::ClaimBeforeRespawn
        }
        Some(MarkerPhase::Ready) | None => PaneAction::Defer,
    }
}

pub(crate) async fn inspect(harness: Harness) -> Result<Option<ExecutableIdentity>> {
    let Some(path) = resolve_launcher(harness) else {
        return Ok(None);
    };
    let version = bounded_output(&path, &["--version"], VERSION_TIMEOUT).await?;
    let metadata = fs::metadata(&path)?;
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_nanos());
    let sha256 = digest_file(&path)?;
    Ok(Some(ExecutableIdentity {
        canonical_path: path,
        version: version.trim().chars().take(160).collect(),
        sha256,
        size: metadata.len(),
        modified_ns,
    }))
}

/// Runs one fixed vendor updater, then records the independently resolved
/// executable identity. "Changed" is based on the binary digest, not updater
/// exit text or a mutable symlink path.
pub(crate) async fn update(
    harness: Harness,
    before: &ExecutableIdentity,
    timeout: Duration,
    lock: &OwnerLock,
) -> Result<ExecutableIdentity> {
    match harness {
        Harness::Claude => {
            bounded_output(&before.canonical_path, &["update"], timeout).await?;
        }
        Harness::Codex => update_codex(timeout, lock).await?,
    }
    inspect(harness)
        .await?
        .context("the updated native launcher could not be resolved")
}

pub(crate) fn changed(before: &ExecutableIdentity, after: &ExecutableIdentity) -> bool {
    before.sha256 != after.sha256
}

pub(crate) fn check_due(last_check_ms: Option<u64>, now_ms: u64, interval_ms: u64) -> bool {
    last_check_ms.is_some_and(|last| now_ms.saturating_sub(last) >= interval_ms)
}

pub(crate) fn advance_generation(current: u64, did_change: bool) -> u64 {
    if did_change {
        current.wrapping_add(1).max(1)
    } else {
        current
    }
}

pub(crate) fn resume_arguments(harness: Harness, session_id: &str) -> Result<Vec<String>> {
    if session_id.len() != 36
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        bail!("native saved conversation id is invalid");
    }
    Ok(match harness {
        Harness::Claude => vec!["--resume".to_owned(), session_id.to_owned()],
        Harness::Codex => vec!["resume".to_owned(), session_id.to_owned()],
    })
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn update_codex(timeout: Duration, lock: &OwnerLock) -> Result<()> {
    let curl = trusted_system_program(Path::new("/usr/bin/curl"))?;
    let shell = trusted_system_program(Path::new("/bin/sh"))?;
    let script = lock.temp_path("codex-install");
    // Pre-create with owner-only permissions; curl opens this exact regular
    // file rather than choosing its mode under the service's ambient umask.
    drop(secure_open(&script)?);
    let result = async {
        bounded_output(
            &curl,
            &[
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--max-time",
                &timeout.as_secs().to_string(),
                "--output",
                script.to_str().context("maintenance path is not UTF-8")?,
                CODEX_INSTALL_URL,
            ],
            timeout,
        )
        .await?;
        reject_symlink_or_unowned(&script)?;
        bounded_output(
            &shell,
            &[script.to_str().context("maintenance path is not UTF-8")?],
            timeout,
        )
        .await?;
        Ok(())
    }
    .await;
    let _ = fs::remove_file(script);
    result
}

async fn bounded_output(program: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .with_context(|| format!("{} exceeded its maintenance deadline", program.display()))??;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} exited with {}: {}",
            program.display(),
            output.status,
            error.trim().chars().take(400).collect::<String>()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn resolve_launcher(harness: Harness) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)?
        .canonicalize()
        .ok()?;
    let metadata = fs::symlink_metadata(&home).ok()?;
    let euid = rustix::process::geteuid().as_raw();
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != euid
        || metadata.permissions().mode() & 0o022 != 0
    {
        return None;
    }
    let candidates = owner_launcher_candidates(&home, harness);
    candidates
        .iter()
        .find_map(|candidate| trusted_owner_executable(candidate, &home, euid))
}

fn owner_launcher_candidates(home: &Path, harness: Harness) -> [PathBuf; 2] {
    let name = harness.name();
    [
        home.join(".local/bin").join(name),
        home.join(format!(".{name}/bin")).join(name),
    ]
}

pub(crate) fn revalidate_launcher(harness: Harness, expected: &Path) -> bool {
    resolve_launcher(harness).as_deref() == Some(expected)
}

fn trusted_owner_executable(candidate: &Path, home: &Path, euid: u32) -> Option<PathBuf> {
    if !candidate.starts_with(home) {
        return None;
    }
    let candidate_meta = fs::symlink_metadata(candidate).ok()?;
    if candidate_meta.uid() != euid {
        return None;
    }
    let canonical = candidate.canonicalize().ok()?;
    if !canonical.starts_with(home) {
        return None;
    }
    let relative = canonical.parent()?.strip_prefix(home).ok()?;
    let mut current = home.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return None;
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current).ok()?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != euid
            || metadata.permissions().mode() & 0o022 != 0
        {
            return None;
        }
    }
    let metadata = fs::symlink_metadata(&canonical).ok()?;
    (metadata.is_file()
        && !metadata.file_type().is_symlink()
        && metadata.uid() == euid
        && metadata.permissions().mode() & 0o111 != 0
        && metadata.permissions().mode() & 0o022 == 0)
        .then_some(canonical)
}

fn trusted_system_program(path: &Path) -> Result<PathBuf> {
    let link_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("required system program {} is unavailable", path.display()))?;
    if link_metadata.uid() != 0 || link_metadata.permissions().mode() & 0o022 != 0 {
        bail!("required system program {} is not trusted", path.display());
    }
    let canonical = path.canonicalize()?;
    let metadata = fs::symlink_metadata(&canonical)?;
    if !canonical.is_absolute()
        || !metadata.is_file()
        || metadata.uid() != 0
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.permissions().mode() & 0o022 != 0
    {
        bail!("required system program {} is not trusted", path.display());
    }
    Ok(canonical)
}

fn digest_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn secure_owner_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!("CLI maintenance state directory is unsafe");
    }
    Ok(())
}

fn secure_open(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    reject_symlink_or_unowned(path)?;
    Ok(file)
}

fn reject_symlink_or_unowned(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!("CLI maintenance file {} is unsafe", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        sync::Mutex,
    };

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum RuntimeEvent {
        Inspect(Harness),
        Collect(Harness),
        Update(Harness),
        Persist,
        ProcessPending(usize),
    }

    struct InjectedRuntime {
        inspections: Mutex<BTreeMap<Harness, VecDeque<Option<ExecutableIdentity>>>>,
        updates:
            Mutex<BTreeMap<Harness, VecDeque<std::result::Result<ExecutableIdentity, String>>>>,
        candidates: BTreeMap<Harness, Vec<PlannedPane>>,
        events: Mutex<Vec<RuntimeEvent>>,
        snapshots: Mutex<Vec<MaintenanceState>>,
        intent_seen_before_update: Mutex<Vec<Harness>>,
        process_resumed: bool,
    }

    impl InjectedRuntime {
        fn new(process_resumed: bool) -> Self {
            Self {
                inspections: Mutex::new(BTreeMap::new()),
                updates: Mutex::new(BTreeMap::new()),
                candidates: BTreeMap::new(),
                events: Mutex::new(Vec::new()),
                snapshots: Mutex::new(Vec::new()),
                intent_seen_before_update: Mutex::new(Vec::new()),
                process_resumed,
            }
        }

        fn push_inspections(&self, harness: Harness, identities: Vec<ExecutableIdentity>) {
            self.inspections
                .lock()
                .unwrap()
                .entry(harness)
                .or_default()
                .extend(identities.into_iter().map(Some));
        }

        fn push_update(
            &self,
            harness: Harness,
            result: std::result::Result<ExecutableIdentity, &str>,
        ) {
            self.updates
                .lock()
                .unwrap()
                .entry(harness)
                .or_default()
                .push_back(result.map_err(str::to_owned));
        }
    }

    impl MaintenanceRuntime for InjectedRuntime {
        fn inspect(&self, harness: Harness) -> MaintenanceFuture<'_, Option<ExecutableIdentity>> {
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(RuntimeEvent::Inspect(harness));
                self.inspections
                    .lock()
                    .unwrap()
                    .entry(harness)
                    .or_default()
                    .pop_front()
                    .context("injected inspection was not configured")
            })
        }

        fn collect(
            &self,
            harness: Harness,
            _before_launcher: PathBuf,
        ) -> MaintenanceFuture<'_, Vec<PlannedPane>> {
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(RuntimeEvent::Collect(harness));
                Ok(self.candidates.get(&harness).cloned().unwrap_or_default())
            })
        }

        fn update(
            &self,
            harness: Harness,
            _before: ExecutableIdentity,
            _timeout: Duration,
        ) -> MaintenanceFuture<'_, ExecutableIdentity> {
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(RuntimeEvent::Update(harness));
                if self
                    .snapshots
                    .lock()
                    .unwrap()
                    .last()
                    .and_then(|state| state.harnesses.get(&harness))
                    .is_some_and(|state| state.intent.is_some())
                {
                    self.intent_seen_before_update.lock().unwrap().push(harness);
                }
                self.updates
                    .lock()
                    .unwrap()
                    .entry(harness)
                    .or_default()
                    .pop_front()
                    .context("injected update was not configured")?
                    .map_err(anyhow::Error::msg)
            })
        }

        fn persist(&self, state: &MaintenanceState) -> Result<()> {
            self.events.lock().unwrap().push(RuntimeEvent::Persist);
            self.snapshots.lock().unwrap().push(state.clone());
            Ok(())
        }

        fn process_pending(
            &self,
            state: MaintenanceState,
            limit: usize,
        ) -> MaintenanceFuture<'_, (MaintenanceState, bool)> {
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(RuntimeEvent::ProcessPending(limit));
                Ok((state, self.process_resumed))
            })
        }
    }

    #[test]
    fn defaults_are_disabled_and_exactly_thirty_minutes() {
        let config = MaintenanceConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.interval_minutes, 30);
        config.validate().unwrap();
    }

    #[test]
    fn marker_round_trip_is_versioned_and_strict() {
        let marker = PendingMarker {
            harness: Harness::Codex,
            generation: 7,
            session_fingerprint: "a".repeat(64),
            phase: MarkerPhase::Ready,
        };
        assert_eq!(PendingMarker::parse(&marker.encode()), Some(marker));
        assert!(PendingMarker::parse(&format!("cu2:codex:7:{}:ready", "A".repeat(64))).is_none());
        assert!(PendingMarker::parse("cu2:codex:7:abc").is_none());
    }

    #[test]
    fn binary_digest_not_version_text_drives_change() {
        let identity = |digest: &str| ExecutableIdentity {
            canonical_path: PathBuf::from("/owner/cli"),
            version: "1.0".to_owned(),
            sha256: digest.to_owned(),
            size: 1,
            modified_ns: 1,
        };
        assert!(!changed(&identity("same"), &identity("same")));
        assert!(changed(&identity("old"), &identity("new")));
    }

    #[test]
    fn invalid_bounds_fail_before_scheduler_start() {
        for config in [
            MaintenanceConfig {
                interval_minutes: 0,
                ..MaintenanceConfig::default()
            },
            MaintenanceConfig {
                update_timeout_seconds: 29,
                ..MaintenanceConfig::default()
            },
            MaintenanceConfig {
                relaunch_limit: 0,
                ..MaintenanceConfig::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn clock_boundary_and_generation_are_exact() {
        assert!(!check_due(None, 1_800_000, 1_800_000));
        assert!(!check_due(Some(1), 1_800_000, 1_800_000));
        assert!(check_due(Some(1), 1_800_001, 1_800_000));
        assert_eq!(advance_generation(8, false), 8);
        assert_eq!(advance_generation(8, true), 9);
    }

    #[test]
    fn native_resume_arguments_are_fixed_for_both_harnesses() {
        let id = "11111111-1111-1111-1111-111111111111";
        assert_eq!(
            resume_arguments(Harness::Claude, id).unwrap(),
            ["--resume", id]
        );
        assert_eq!(
            resume_arguments(Harness::Codex, id).unwrap(),
            ["resume", id]
        );
        assert!(resume_arguments(Harness::Claude, "$(unsafe)").is_err());
    }

    #[test]
    fn owner_candidates_do_not_depend_on_path_or_ssh_environment() {
        let home = Path::new("/Users/ryan");
        assert_eq!(
            owner_launcher_candidates(home, Harness::Claude),
            [
                PathBuf::from("/Users/ryan/.local/bin/claude"),
                PathBuf::from("/Users/ryan/.claude/bin/claude")
            ]
        );
    }

    #[test]
    fn process_lock_and_state_survive_a_new_scheduler_instance() {
        let root = std::env::temp_dir().join(format!(
            "atmux-maintenance-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let first = OwnerLock::try_acquire_in(root.clone()).unwrap().unwrap();
        assert!(OwnerLock::try_acquire_in(root.clone()).unwrap().is_none());
        let mut state = MaintenanceState {
            last_check_ms: Some(42),
            ..MaintenanceState::default()
        };
        state.harness_mut(Harness::Codex).last_error = Some("bounded failure".to_owned());
        first.store(&state).unwrap();
        drop(first);
        let restarted = OwnerLock::try_acquire_in(root.clone()).unwrap().unwrap();
        let loaded = restarted.load().unwrap();
        assert_eq!(loaded.last_check_ms, Some(42));
        assert_eq!(
            loaded.harnesses[&Harness::Codex].last_error.as_deref(),
            Some("bounded failure")
        );
        drop(restarted);
        let _ = fs::remove_dir_all(root);
    }

    fn identity(digest: &str) -> ExecutableIdentity {
        ExecutableIdentity {
            canonical_path: PathBuf::from("/owner/.local/bin/agent"),
            version: digest.to_owned(),
            sha256: digest.to_owned(),
            size: 1,
            modified_ns: 1,
        }
    }

    fn planned() -> PlannedPane {
        PlannedPane {
            pane_id: "%7".to_owned(),
            session_fingerprint: "d".repeat(64),
            mutation_sequence: 3,
            profile: "Default".to_owned(),
            mode_id: "sol-high-fast".to_owned(),
        }
    }

    fn cycle_settings(now_ms: u64, relaunch_limit: usize) -> CycleSettings {
        CycleSettings {
            now_ms,
            interval_ms: 1_000,
            update_timeout: Duration::from_secs(30),
            relaunch_limit,
        }
    }

    #[tokio::test]
    async fn injected_production_cycle_orders_baseline_intent_failure_and_processing() {
        let first = InjectedRuntime::new(false);
        first.push_inspections(Harness::Claude, vec![identity("claude-v1")]);
        first.push_inspections(Harness::Codex, vec![identity("codex-v1")]);
        let (state, resumed) =
            run_maintenance_cycle(&first, MaintenanceState::default(), cycle_settings(10, 3))
                .await
                .unwrap();
        assert!(!resumed);
        assert_eq!(state.last_check_ms, Some(10));
        assert_eq!(state.harnesses[&Harness::Claude].generation, 0);
        assert_eq!(state.harnesses[&Harness::Codex].generation, 0);
        assert_eq!(
            *first.events.lock().unwrap(),
            [
                RuntimeEvent::Inspect(Harness::Claude),
                RuntimeEvent::Inspect(Harness::Codex),
                RuntimeEvent::ProcessPending(3),
                RuntimeEvent::Persist,
            ],
            "the first delayed pass only inspects and persists baselines"
        );

        let mut due = InjectedRuntime::new(true);
        due.candidates.insert(Harness::Claude, vec![planned()]);
        due.candidates.insert(Harness::Codex, vec![planned()]);
        due.push_inspections(
            Harness::Claude,
            vec![identity("claude-v1"), identity("claude-v1")],
        );
        due.push_inspections(Harness::Codex, vec![identity("codex-v1")]);
        due.push_update(Harness::Claude, Err("bounded vendor failure"));
        due.push_update(Harness::Codex, Ok(identity("codex-v2")));
        let (state, resumed) = run_maintenance_cycle(&due, state, cycle_settings(1_010, 1))
            .await
            .unwrap();
        assert!(resumed);
        assert_eq!(
            *due.intent_seen_before_update.lock().unwrap(),
            [Harness::Claude, Harness::Codex],
            "each vendor starts only after its exact intent snapshot is durable"
        );
        assert_eq!(state.harnesses[&Harness::Claude].generation, 0);
        assert!(
            state.harnesses[&Harness::Claude]
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("bounded vendor failure"))
        );
        assert_eq!(state.harnesses[&Harness::Codex].generation, 1);
        assert_eq!(state.harnesses[&Harness::Codex].pending, vec![planned()]);
        assert_eq!(
            due.events.lock().unwrap().last(),
            Some(&RuntimeEvent::Persist)
        );
        assert!(
            due.events
                .lock()
                .unwrap()
                .contains(&RuntimeEvent::ProcessPending(1))
        );
    }

    #[tokio::test]
    async fn injected_cycle_recovers_restart_after_installer_before_generation_commit() {
        let plan = planned();
        let mut state = MaintenanceState {
            last_check_ms: Some(10),
            ..MaintenanceState::default()
        };
        let harness_state = state.harness_mut(Harness::Claude);
        harness_state.applied = Some(identity("old"));
        harness_state.prepare_intent(identity("old"), vec![plan.clone()]);
        let persisted = serde_json::to_vec(&state).unwrap();
        let restarted = serde_json::from_slice(&persisted).unwrap();

        let runtime = InjectedRuntime::new(false);
        runtime.push_inspections(Harness::Claude, vec![identity("new")]);
        let (state, _) = run_maintenance_cycle(&runtime, restarted, cycle_settings(11, 2))
            .await
            .unwrap();
        assert_eq!(state.harnesses[&Harness::Claude].generation, 1);
        assert_eq!(state.harnesses[&Harness::Claude].pending, vec![plan]);
        assert!(
            !runtime
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, RuntimeEvent::Update(_) | RuntimeEvent::Collect(_)))
        );
    }

    #[test]
    fn first_run_persists_baseline_and_no_change_never_generates_restart() {
        let mut state = HarnessState::default();
        state.prepare_intent(identity("one"), vec![planned()]);
        assert_eq!(state.applied.as_ref().unwrap().sha256, "one");
        assert!(
            state.intent.is_some(),
            "intent exists before vendor execution"
        );
        assert_eq!(
            state.reconcile(identity("one"), None),
            ReconcileResult::NoChange
        );
        assert_eq!(state.generation, 0);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn background_update_between_checks_creates_generation_without_vendor_delta() {
        let mut state = HarnessState {
            applied: Some(identity("old")),
            ..HarnessState::default()
        };
        // The provider changed the executable before this scheduled check.
        state.prepare_intent(identity("background-new"), vec![planned()]);
        assert_eq!(
            state.reconcile(identity("background-new"), None),
            ReconcileResult::Changed { generation: 1 }
        );
        assert_eq!(state.pending, vec![planned()]);
    }

    #[test]
    fn restart_after_installer_mutation_recovers_pre_generation_intent() {
        let mut before_crash = HarnessState {
            applied: Some(identity("old")),
            ..HarnessState::default()
        };
        before_crash.prepare_intent(identity("old"), vec![planned()]);
        let encoded = serde_json::to_vec(&before_crash).unwrap();
        let mut restarted: HarnessState = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            restarted.reconcile(
                identity("new"),
                Some("previous updater interrupted".to_owned())
            ),
            ReconcileResult::Changed { generation: 1 }
        );
        assert_eq!(restarted.pending, vec![planned()]);
    }

    #[test]
    fn update_failure_isolated_per_harness_and_does_not_invent_change() {
        let mut claude = HarnessState {
            applied: Some(identity("claude-old")),
            ..HarnessState::default()
        };
        claude.prepare_intent(identity("claude-old"), vec![planned()]);
        assert_eq!(
            claude.reconcile(identity("claude-old"), Some("network deadline".to_owned())),
            ReconcileResult::NoChange
        );
        assert_eq!(claude.generation, 0);
        let mut codex = HarnessState {
            applied: Some(identity("codex-old")),
            ..HarnessState::default()
        };
        codex.prepare_intent(identity("codex-old"), vec![planned()]);
        assert_eq!(
            codex.reconcile(identity("codex-new"), None),
            ReconcileResult::Changed { generation: 1 }
        );
    }

    #[test]
    fn partial_marking_retries_and_active_or_approval_state_defers() {
        let plan = planned();
        let transient = PaneObservation {
            exists: true,
            identity_definitively_stale: false,
            session_fingerprint: Some(plan.session_fingerprint.clone()),
            mutation_sequence: Some(plan.mutation_sequence),
            exact_idle: false,
        };
        assert_eq!(
            pane_action(&plan, None, &transient, Harness::Claude, 2),
            PaneAction::MaterializeReady,
            "a crash before marking is repaired on the next poll"
        );
        let ready = PendingMarker {
            harness: Harness::Claude,
            generation: 2,
            session_fingerprint: plan.session_fingerprint.clone(),
            phase: MarkerPhase::Ready,
        };
        assert_eq!(
            pane_action(&plan, Some(&ready), &transient, Harness::Claude, 2),
            PaneAction::Defer,
            "working, approval, and unrecognized transient states retain the marker"
        );
        let idle = PaneObservation {
            exact_idle: true,
            ..transient
        };
        assert_eq!(
            pane_action(&plan, Some(&ready), &idle, Harness::Claude, 2),
            PaneAction::ClaimBeforeRespawn
        );
    }

    #[test]
    fn claimed_respawn_is_at_most_once_and_user_mutation_invalidates_plan() {
        let plan = planned();
        let claimed = PendingMarker {
            harness: Harness::Codex,
            generation: 9,
            session_fingerprint: plan.session_fingerprint.clone(),
            phase: MarkerPhase::Claimed,
        };
        let idle = PaneObservation {
            exists: true,
            identity_definitively_stale: false,
            session_fingerprint: Some(plan.session_fingerprint.clone()),
            mutation_sequence: Some(plan.mutation_sequence),
            exact_idle: true,
        };
        assert_eq!(
            pane_action(&plan, Some(&claimed), &idle, Harness::Codex, 9),
            PaneAction::AlreadyClaimed,
            "an ambiguous respawn is never retried"
        );
        let mutated = PaneObservation {
            mutation_sequence: Some(plan.mutation_sequence + 1),
            ..idle.clone()
        };
        assert_eq!(
            pane_action(&plan, Some(&claimed), &mutated, Harness::Codex, 9),
            PaneAction::AlreadyClaimed,
            "a claimed destructive boundary remains at-most-once even after a crash"
        );
        let ready = PendingMarker {
            phase: MarkerPhase::Ready,
            ..claimed.clone()
        };
        assert_eq!(
            pane_action(&plan, Some(&ready), &mutated, Harness::Codex, 9),
            PaneAction::Forget,
            "a human mutation invalidates a not-yet-claimed plan"
        );
        assert_eq!(
            pane_action(&plan, Some(&claimed), &idle, Harness::Codex, 10),
            PaneAction::MaterializeReady,
            "an older generation tombstone must not suppress a later CLI update"
        );
    }

    #[test]
    fn cross_process_pane_lock_excludes_overlapping_mutations() {
        let root = std::env::temp_dir().join(format!(
            "atmux-pane-lock-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let first = PaneProcessLock::try_acquire_in(&root, "%7")
            .unwrap()
            .unwrap();
        assert!(
            PaneProcessLock::try_acquire_in(&root, "%7")
                .unwrap()
                .is_none()
        );
        assert!(
            PaneProcessLock::try_acquire_in(&root, "%8")
                .unwrap()
                .is_some()
        );
        drop(first);
        assert!(
            PaneProcessLock::try_acquire_in(&root, "%7")
                .unwrap()
                .is_some()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn injected_scheduler_restart_active_to_idle_and_ambiguous_respawn_flow() {
        // First delayed pass: establish a baseline and do not invent a change.
        let mut harness = HarnessState::default();
        assert_eq!(
            harness.reconcile(identity("v1"), None),
            ReconcileResult::NoChange
        );
        assert_eq!(harness.generation, 0);

        // Due pass: preflight an active exact native pane, persist intent, then
        // let the injected updater change the executable.
        let plan = planned();
        harness.prepare_intent(identity("v1"), vec![plan.clone()]);
        let persisted_intent = serde_json::to_vec(&harness).unwrap();
        let mut after_vendor_restart: HarnessState =
            serde_json::from_slice(&persisted_intent).unwrap();
        assert_eq!(
            after_vendor_restart.reconcile(identity("v2"), None),
            ReconcileResult::Changed { generation: 1 }
        );

        // Crash before materializing a marker: persisted pending repairs it.
        let persisted_generation = serde_json::to_vec(&after_vendor_restart).unwrap();
        let restarted: HarnessState = serde_json::from_slice(&persisted_generation).unwrap();
        let active = PaneObservation {
            exists: true,
            identity_definitively_stale: false,
            session_fingerprint: Some(plan.session_fingerprint.clone()),
            mutation_sequence: Some(plan.mutation_sequence),
            exact_idle: false,
        };
        assert_eq!(
            pane_action(&plan, None, &active, Harness::Codex, restarted.generation),
            PaneAction::MaterializeReady
        );
        let ready = PendingMarker {
            harness: Harness::Codex,
            generation: restarted.generation,
            session_fingerprint: plan.session_fingerprint.clone(),
            phase: MarkerPhase::Ready,
        };
        assert_eq!(
            pane_action(
                &plan,
                Some(&ready),
                &active,
                Harness::Codex,
                restarted.generation
            ),
            PaneAction::Defer
        );

        // The exact composer becomes idle. Production first persists Claimed,
        // then crosses tmux's ambiguous destructive respawn boundary.
        let idle = PaneObservation {
            exact_idle: true,
            ..active
        };
        assert_eq!(
            pane_action(
                &plan,
                Some(&ready),
                &idle,
                Harness::Codex,
                restarted.generation
            ),
            PaneAction::ClaimBeforeRespawn
        );
        let claimed = PendingMarker {
            phase: MarkerPhase::Claimed,
            ..ready
        };
        assert_eq!(
            pane_action(
                &plan,
                Some(&claimed),
                &idle,
                Harness::Codex,
                restarted.generation
            ),
            PaneAction::AlreadyClaimed,
            "an injected ambiguous respawn failure cannot be delivered twice"
        );
    }
}
