//! Owner-local native Claude/Codex CLI maintenance primitives.
//!
//! The control plane owns pane selection and tmux mutation. This module owns
//! the cross-process lock, durable update generations, executable identity,
//! and the two fixed vendor update protocols. No request value can become a
//! program, URL, argument, or environment assignment here.

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    future::Future,
    io::{Read as _, Write as _},
    os::unix::ffi::OsStringExt as _,
    os::unix::fs::{
        DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
    },
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use directories::ProjectDirs;
use fs2::FileExt as _;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawMode};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::process::Command;

const STATE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 256 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const CODEX_INSTALL_URL: &str = "https://chatgpt.com/codex/install.sh";
const CODEX_INSTALL_COMMAND: &str = "umask 077 && exec /bin/sh \"$1\"";
const CODEX_INSTALL_ARGV0: &str = "atmux-codex-installer";
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

enum TempArtifact {
    File(PathBuf),
    Directory(PathBuf),
}

#[derive(Default)]
struct TempCleanup(Vec<TempArtifact>);

impl TempCleanup {
    fn file(&mut self, path: &Path) {
        self.0.push(TempArtifact::File(path.to_path_buf()));
    }

    fn directory(&mut self, path: &Path) {
        self.0.push(TempArtifact::Directory(path.to_path_buf()));
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        for artifact in self.0.iter().rev() {
            match artifact {
                TempArtifact::File(path) => {
                    let _ = fs::remove_file(path);
                }
                TempArtifact::Directory(path) => {
                    let _ = fs::remove_dir(path);
                }
            }
        }
    }
}

struct CodexUpdateFiles {
    script: PathBuf,
    curl_home: PathBuf,
    curl_config: PathBuf,
    _cleanup: TempCleanup,
}

impl CodexUpdateFiles {
    fn prepare(lock: &OwnerLock) -> Result<Self> {
        let mut cleanup = TempCleanup::default();
        let (script, script_file) = lock.create_temp_file("codex-install")?;
        drop(script_file);
        cleanup.file(&script);

        let curl_home = lock.create_temp_directory("curl-home")?;
        cleanup.directory(&curl_home);
        let curl_config = curl_home.join(".curlrc");
        cleanup.file(&curl_config);
        drop(create_secure_empty_file(&curl_config)?);

        Ok(Self {
            script,
            curl_home,
            curl_config,
            _cleanup: cleanup,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexPermissionStage {
    DirectoryOpened,
    TargetOpened,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SymlinkIdentity {
    device: u64,
    inode: u64,
    target: PathBuf,
}

struct PublicLauncherState {
    local: DirectoryIdentity,
    bin: DirectoryIdentity,
    link: SymlinkIdentity,
}

struct PackageChain<'a> {
    home: &'a File,
    euid: u32,
    codex: DirectoryIdentity,
    names: Vec<OsString>,
    identities: Vec<DirectoryIdentity>,
}

impl PackageChain<'_> {
    fn repair_child(
        &mut self,
        parent: &File,
        name: &OsStr,
        path: &Path,
        stage: &mut impl FnMut(CodexPermissionStage, &Path) -> Result<()>,
    ) -> Result<File> {
        let directory = open_owner_directory(parent, name, path)?;
        stage(CodexPermissionStage::DirectoryOpened, path)?;
        self.names.push(name.to_os_string());
        self.identities
            .push(validate_owner_directory(&directory, path, self.euid, true)?);
        self.reopen()?;
        Ok(directory)
    }

    fn reopen(&self) -> Result<File> {
        reopen_package_chain(
            self.home,
            &self.codex,
            &self.names,
            &self.identities,
            self.euid,
        )
    }
}

/// Repairs only package-directory permissions left behind by an earlier
/// official Codex install that inherited a permissive service umask. Lookup
/// is anchored at a trusted HOME descriptor and follows the installer's exact
/// fixed layout without canonicalizing through the writable tree. `.codex`,
/// `.local`, files, and symlinks are validated but never chmodded.
fn reconcile_codex_package_permissions(home: &Path) -> Result<bool> {
    reconcile_codex_package_permissions_with(home, |_, _| Ok(()))
}

fn reconcile_codex_package_permissions_with(
    home: &Path,
    mut stage: impl FnMut(CodexPermissionStage, &Path) -> Result<()>,
) -> Result<bool> {
    let euid = rustix::process::geteuid().as_raw();
    let home_directory = File::open(home)?;
    let _home_identity = validate_owner_directory(&home_directory, home, euid, false)?;
    let Some(public) = validate_public_launcher(&home_directory, home, euid, &mut stage)? else {
        return Ok(false);
    };
    repair_codex_package_tree(&home_directory, home, euid, &mut stage)?;
    validate_public_launcher_final(&home_directory, home, euid, &public)?;
    Ok(true)
}

fn validate_public_launcher(
    home_directory: &File,
    home: &Path,
    euid: u32,
    stage: &mut impl FnMut(CodexPermissionStage, &Path) -> Result<()>,
) -> Result<Option<PublicLauncherState>> {
    let local_path = home.join(".local");
    let Ok(local_directory) =
        open_owner_directory(home_directory, OsStr::new(".local"), &local_path)
    else {
        return Ok(None);
    };
    let local_identity = validate_owner_directory(&local_directory, &local_path, euid, false)?;
    let local_bin_path = local_path.join("bin");
    let Ok(local_bin_directory) =
        open_owner_directory(&local_directory, OsStr::new("bin"), &local_bin_path)
    else {
        return Ok(None);
    };
    let local_bin_identity =
        validate_owner_directory(&local_bin_directory, &local_bin_path, euid, false)?;
    stage(CodexPermissionStage::DirectoryOpened, &local_bin_path)?;
    let expected_launcher_target = home.join(".codex/packages/standalone/current/bin/codex");
    let Ok(launcher_link) = validated_symlink(&local_bin_directory, OsStr::new("codex"), euid)
    else {
        return Ok(None);
    };
    if launcher_link.target != expected_launcher_target {
        return Ok(None);
    }
    Ok(Some(PublicLauncherState {
        local: local_identity,
        bin: local_bin_identity,
        link: launcher_link,
    }))
}

fn repair_codex_package_tree(
    home_directory: &File,
    home: &Path,
    euid: u32,
    stage: &mut impl FnMut(CodexPermissionStage, &Path) -> Result<()>,
) -> Result<()> {
    let codex_path = home.join(".codex");
    let codex_directory = open_owner_directory(home_directory, OsStr::new(".codex"), &codex_path)?;
    let codex_identity = validate_owner_directory(&codex_directory, &codex_path, euid, false)?;
    let mut chain = PackageChain {
        home: home_directory,
        euid,
        codex: codex_identity,
        names: Vec::new(),
        identities: Vec::new(),
    };
    let packages_path = codex_path.join("packages");
    let packages_directory = chain.repair_child(
        &codex_directory,
        OsStr::new("packages"),
        &packages_path,
        stage,
    )?;

    let standalone_path = packages_path.join("standalone");
    let standalone_directory = chain.repair_child(
        &packages_directory,
        OsStr::new("standalone"),
        &standalone_path,
        stage,
    )?;

    // Only this expected symlink is accepted inside the package ancestry. Its
    // absolute target must name exactly one bounded release directory.
    let current_link = validated_symlink(&standalone_directory, OsStr::new("current"), euid)?;
    let releases_path = standalone_path.join("releases");
    let release_name = codex_release_name(&current_link, &releases_path)?;
    let releases_root = chain.repair_child(
        &standalone_directory,
        OsStr::new("releases"),
        &releases_path,
        stage,
    )?;
    let release_path = releases_path.join(&release_name);
    let version_directory =
        chain.repair_child(&releases_root, &release_name, &release_path, stage)?;
    let bin_path = release_path.join("bin");
    let bin_directory =
        chain.repair_child(&version_directory, OsStr::new("bin"), &bin_path, stage)?;

    let target_path = bin_path.join("codex");
    let target = open_owner_executable(&bin_directory, OsStr::new("codex"), &target_path)?;
    stage(CodexPermissionStage::TargetOpened, &target_path)?;
    let target_identity = validate_owner_executable(&target, &target_path, euid)?;

    let final_directory = chain.reopen()?;
    let final_target = open_owner_executable(&final_directory, OsStr::new("codex"), &target_path)?;
    let final_identity = validate_owner_executable(&final_target, &target_path, euid)?;
    if final_identity != target_identity
        || validated_symlink(&standalone_directory, OsStr::new("current"), euid)? != current_link
    {
        bail!("Codex package layout changed during permission repair");
    }
    Ok(())
}

fn codex_release_name(current: &SymlinkIdentity, releases: &Path) -> Result<OsString> {
    let relative = current.target.strip_prefix(releases).map_err(|_| {
        anyhow::anyhow!("Codex current link does not target the fixed releases directory")
    })?;
    let mut components = relative.components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(release)), None)
            if !release.is_empty()
                && release.as_encoded_bytes().len() <= 160
                && release.as_encoded_bytes().iter().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
                }) =>
        {
            Ok(release.to_os_string())
        }
        _ => bail!("Codex current link has an invalid release component"),
    }
}

fn validate_public_launcher_final(
    home_directory: &File,
    home: &Path,
    euid: u32,
    expected: &PublicLauncherState,
) -> Result<()> {
    let local_path = home.join(".local");
    let local_bin_path = local_path.join("bin");
    let final_local_directory =
        open_owner_directory(home_directory, OsStr::new(".local"), &local_path)?;
    let final_local_identity =
        validate_owner_directory(&final_local_directory, &local_path, euid, false)?;
    let final_local_bin_directory =
        open_owner_directory(&final_local_directory, OsStr::new("bin"), &local_bin_path)?;
    let final_local_bin_identity =
        validate_owner_directory(&final_local_bin_directory, &local_bin_path, euid, false)?;
    if final_local_identity != expected.local
        || final_local_bin_identity != expected.bin
        || validated_symlink(&final_local_bin_directory, OsStr::new("codex"), euid)?
            != expected.link
    {
        bail!("Codex public launcher changed during package permission repair");
    }
    Ok(())
}

fn reopen_package_chain(
    home: &File,
    codex_identity: &DirectoryIdentity,
    names: &[OsString],
    identities: &[DirectoryIdentity],
    euid: u32,
) -> Result<File> {
    let mut current = open_owner_directory(home, OsStr::new(".codex"), Path::new(".codex"))?;
    if validate_owner_directory(&current, Path::new(".codex"), euid, false)? != *codex_identity {
        bail!("Codex package anchor changed during permission repair");
    }
    for (name, expected) in names.iter().zip(identities) {
        current = open_owner_directory(&current, name, Path::new(".codex/packages"))?;
        if validate_owner_directory(&current, Path::new(".codex/packages"), euid, false)?
            != *expected
        {
            bail!("Codex package directory changed during permission repair");
        }
    }
    Ok(current)
}

fn open_owner_directory(parent: &File, name: &std::ffi::OsStr, path: &Path) -> Result<File> {
    rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY,
        Mode::empty(),
    )
    .map(File::from)
    .with_context(|| {
        format!(
            "could not safely open Codex package directory {}",
            path.display()
        )
    })
}

fn open_owner_executable(parent: &File, name: &std::ffi::OsStr, path: &Path) -> Result<File> {
    let before = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if !FileType::from_raw_mode(before.st_mode).is_file() {
        bail!(
            "Codex package launcher {} is not a regular file",
            path.display()
        );
    }
    let file: File = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NOCTTY,
        Mode::empty(),
    )?
    .into();
    let metadata = file.metadata()?;
    if before.st_dev as u64 != metadata.dev() || before.st_ino as u64 != metadata.ino() {
        bail!(
            "Codex package launcher {} changed while opening",
            path.display()
        );
    }
    Ok(file)
}

fn validated_symlink(parent: &File, name: &std::ffi::OsStr, euid: u32) -> Result<SymlinkIdentity> {
    let before = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if !FileType::from_raw_mode(before.st_mode).is_symlink()
        || before.st_uid != euid
        || before.st_nlink != 1
    {
        bail!("Codex installer link is not a safe euid-owned symlink");
    }
    let target = PathBuf::from(OsString::from_vec(
        rustix::fs::readlinkat(parent, name, Vec::new())?.into_bytes(),
    ));
    let after = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if before.st_dev != after.st_dev
        || before.st_ino != after.st_ino
        || before.st_uid != after.st_uid
        || before.st_nlink != after.st_nlink
        || !FileType::from_raw_mode(after.st_mode).is_symlink()
    {
        bail!("Codex installer link changed while validating");
    }
    Ok(SymlinkIdentity {
        device: before.st_dev as u64,
        inode: before.st_ino as u64,
        target,
    })
}

fn validate_owner_directory(
    directory: &File,
    path: &Path,
    euid: u32,
    repair_writable: bool,
) -> Result<DirectoryIdentity> {
    let metadata = directory.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != euid
        || metadata.permissions().mode() & 0o700 != 0o700
    {
        bail!(
            "Codex package directory {} is not an euid-owned searchable directory",
            path.display()
        );
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        if !repair_writable {
            bail!(
                "Codex package parent {} is writable by another uid; inspect it manually",
                path.display()
            );
        }
        let repaired = checked_raw_mode(metadata.permissions().mode() & 0o7777 & !0o022)?;
        rustix::fs::fchmod(directory, Mode::from_raw_mode(repaired))?;
    }
    let verified = directory.metadata()?;
    if !verified.is_dir()
        || verified.uid() != euid
        || verified.dev() != metadata.dev()
        || verified.ino() != metadata.ino()
        || verified.permissions().mode() & 0o022 != 0
    {
        bail!(
            "Codex package directory {} changed during permission repair",
            path.display()
        );
    }
    Ok(DirectoryIdentity {
        device: verified.dev(),
        inode: verified.ino(),
    })
}

fn checked_mode<T>(mode: u32) -> Option<T>
where
    T: TryFrom<u32>,
{
    T::try_from(mode).ok()
}

fn checked_raw_mode(mode: u32) -> Result<RawMode> {
    checked_mode(mode).context("Codex package mode cannot be represented on this platform")
}

fn validate_owner_executable(file: &File, path: &Path, euid: u32) -> Result<DirectoryIdentity> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != euid
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.permissions().mode() & 0o022 != 0
    {
        bail!(
            "Codex package launcher {} changed or is not a safe executable",
            path.display()
        );
    }
    Ok(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
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

    fn create_temp_directory(&self, label: &str) -> Result<PathBuf> {
        create_exclusive_temp_directory(std::iter::repeat_with(|| self.temp_path(label)).take(16))
    }

    fn create_temp_file(&self, label: &str) -> Result<(PathBuf, File)> {
        create_exclusive_temp_file(std::iter::repeat_with(|| self.temp_path(label)).take(16))
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
    let Some(path) = resolve_launcher_with_recovery(harness)? else {
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
    let files = CodexUpdateFiles::prepare(lock)?;
    async {
        let timeout_seconds = timeout.as_secs().to_string();
        let script_path = files
            .script
            .to_str()
            .context("maintenance path is not UTF-8")?;
        let curl_arguments = codex_download_arguments(&timeout_seconds, script_path);
        bounded_system_output(
            Path::new("/usr/bin/curl"),
            &curl_arguments,
            timeout,
            &files.curl_home,
        )
        .await?;
        reject_symlink_or_unowned(&files.script)?;
        reject_secure_empty_file(&files.curl_config)?;
        let installer_arguments = codex_installer_arguments(script_path)?;
        bounded_system_output(
            Path::new("/bin/sh"),
            &installer_arguments,
            timeout,
            &files.curl_home,
        )
        .await?;
        let home = trusted_owner_home().context("owner HOME is not trusted after Codex update")?;
        reconcile_codex_package_permissions(&home).with_context(|| {
            format!(
                "Codex was installed, but package permissions could not be reconciled safely under {}",
                home.join(".codex/packages").display()
            )
        })?;
        Ok(())
    }
    .await
}

fn codex_installer_arguments(script: &str) -> Result<[&str; 4]> {
    if !Path::new(script).is_absolute() || script.starts_with('-') {
        bail!("Codex installer path must be absolute and non-option");
    }
    Ok(["-c", CODEX_INSTALL_COMMAND, CODEX_INSTALL_ARGV0, script])
}

fn codex_download_arguments<'a>(timeout_seconds: &'a str, script: &'a str) -> [&'a str; 14] {
    [
        // curl only honors this switch before every other argument. It blocks
        // owner-controlled ~/.curlrc and CURL_HOME configuration.
        "--disable",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--max-time",
        timeout_seconds,
        "--output",
        script,
        CODEX_INSTALL_URL,
    ]
}

/// Re-resolves and validates one of the two fixed system launchers immediately
/// before constructing its child process. The trusted namespace makes the
/// remaining validation-to-exec window unavailable to an unprivileged service
/// user; a privileged/root actor is outside this boundary.
async fn bounded_system_output(
    requested: &Path,
    args: &[&str],
    timeout: Duration,
    curl_home: &Path,
) -> Result<String> {
    let (program, command) = validated_system_command(requested, args, curl_home)?;
    run_bounded_output(command, &program, timeout).await
}

async fn bounded_output(program: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    run_bounded_output(command, program, timeout).await
}

async fn run_bounded_output(
    mut command: Command,
    program: &Path,
    timeout: Duration,
) -> Result<String> {
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
    let home = trusted_owner_home()?;
    resolve_launcher_at(&home, harness)
}

fn resolve_launcher_at(home: &Path, harness: Harness) -> Option<PathBuf> {
    let euid = rustix::process::geteuid().as_raw();
    let candidates = owner_launcher_candidates(home, harness);
    candidates
        .iter()
        .find_map(|candidate| trusted_owner_executable(candidate, home, euid))
}

fn resolve_launcher_with_recovery(harness: Harness) -> Result<Option<PathBuf>> {
    let Some(home) = trusted_owner_home() else {
        return Ok(None);
    };
    resolve_launcher_with_recovery_at(&home, harness)
}

fn resolve_launcher_with_recovery_at(home: &Path, harness: Harness) -> Result<Option<PathBuf>> {
    let resolved = resolve_launcher_at(home, harness);
    if resolved.is_some() || harness != Harness::Codex {
        return Ok(resolved);
    }
    if !reconcile_codex_package_permissions(home)? {
        return Ok(None);
    }
    Ok(resolve_launcher_at(home, harness))
}

fn trusted_owner_home() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)?
        .canonicalize()
        .ok()?;
    let euid = rustix::process::geteuid().as_raw();
    let nodes = home
        .ancestors()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|path| system_node(path, None))
        .collect::<Result<Vec<_>>>()
        .ok()?;
    owner_home_nodes_trusted(&nodes, euid).then_some(home)
}

fn owner_home_nodes_trusted(nodes: &[SystemNode], euid: u32) -> bool {
    nodes.last().is_some_and(|node| node.uid == euid)
        && nodes.iter().all(|node| {
            node.path.is_absolute()
                && node.kind == SystemNodeKind::Directory
                && (node.uid == 0 || node.uid == euid)
                && node.mode & 0o022 == 0
        })
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SystemNodeKind {
    Directory,
    Symlink,
    File,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SystemNode {
    path: PathBuf,
    kind: SystemNodeKind,
    uid: u32,
    mode: u32,
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    link_target: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SystemProgramResolution {
    canonical_path: PathBuf,
    nodes: Vec<SystemNode>,
}

trait SystemProgramResolver {
    fn resolve(&self, requested: &Path) -> Result<SystemProgramResolution>;
}

struct OsSystemProgramResolver;

impl SystemProgramResolver for OsSystemProgramResolver {
    fn resolve(&self, requested: &Path) -> Result<SystemProgramResolution> {
        resolve_system_program(requested)
    }
}

trait SystemPathAccess {
    fn node(&self, path: &Path) -> Result<SystemNode>;
    fn read_link(&self, path: &Path) -> Result<PathBuf>;
}

struct OsSystemPathAccess;

impl SystemPathAccess for OsSystemPathAccess {
    fn node(&self, path: &Path) -> Result<SystemNode> {
        system_node(path, None)
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf> {
        Ok(fs::read_link(path)?)
    }
}

/// Only the hard-coded vendor updater dependencies can cross this boundary.
/// Each resolution records the complete symlink namespace and canonical
/// ancestry. Comparing two consecutive resolutions detects changes during
/// validation, but is not an atomic-exec primitive. Security comes from every
/// traversed object being rooted in a root-owned, non-group/world-writable
/// namespace; replacing it requires root. Root compromise, malicious mounts,
/// and platform ACL semantics not represented by POSIX mode bits are outside
/// this service-user boundary. The final result is used immediately by
/// `Command`, leaving only that privileged-attacker validation-to-exec window.
fn trusted_system_program(path: &Path) -> Result<PathBuf> {
    trusted_system_program_with(path, &OsSystemProgramResolver)
}

fn validated_system_command(
    path: &Path,
    args: &[&str],
    curl_home: &Path,
) -> Result<(PathBuf, Command)> {
    let home = trusted_owner_home().context("owner HOME is not trusted for CLI maintenance")?;
    reject_secure_owner_directory(curl_home)?;
    reject_secure_empty_file(&curl_home.join(".curlrc"))?;
    // System-program resolution is deliberately the final filesystem action
    // before Command construction.
    let program = trusted_system_program(path)?;
    Ok(system_command(program, args, &home, curl_home))
}

#[cfg(test)]
fn validated_system_command_with(
    path: &Path,
    args: &[&str],
    resolver: &impl SystemProgramResolver,
    curl_home: &Path,
) -> Result<(PathBuf, Command)> {
    let home = trusted_owner_home().context("owner HOME is not trusted for CLI maintenance")?;
    reject_secure_owner_directory(curl_home)?;
    reject_secure_empty_file(&curl_home.join(".curlrc"))?;
    let program = trusted_system_program_with(path, resolver)?;
    // Keep command construction adjacent to the second validation pass. No
    // network, filesystem, or async operation belongs in this gap.
    Ok(system_command(program, args, &home, curl_home))
}

fn system_command(
    program: PathBuf,
    args: &[&str],
    home: &Path,
    curl_home: &Path,
) -> (PathBuf, Command) {
    let mut command = Command::new(&program);
    command
        .args(args)
        // Do not let CURL_HOME, CURL_CA_BUNDLE, SSL_CERT_FILE, ENV, BASH_ENV,
        // shell functions, or a service PATH alter these privileged updater
        // dependencies. HOME is the independently validated owner directory
        // required by the official installer; PATH is fixed to system tools.
        .env_clear()
        .env("HOME", home)
        // /usr/sbin is required by install.sh's macOS Rosetta `sysctl` probe.
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("CURL_HOME", curl_home)
        .env("CODEX_NON_INTERACTIVE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    (program, command)
}

fn trusted_system_program_with(
    path: &Path,
    resolver: &impl SystemProgramResolver,
) -> Result<PathBuf> {
    if path != Path::new("/bin/sh") && path != Path::new("/usr/bin/curl") {
        bail!("required system program path is not allow-listed");
    }
    let first = resolver
        .resolve(path)
        .with_context(|| format!("required system program {} is unavailable", path.display()))?;
    validate_system_resolution(path, &first)?;
    let second = resolver.resolve(path).with_context(|| {
        format!(
            "required system program {} changed during validation",
            path.display()
        )
    })?;
    validate_system_resolution(path, &second)?;
    if first != second {
        bail!(
            "required system program {} changed during validation",
            path.display()
        );
    }
    Ok(second.canonical_path)
}

fn validate_system_resolution(path: &Path, resolution: &SystemProgramResolution) -> Result<()> {
    if !resolution.canonical_path.is_absolute() || resolution.nodes.is_empty() {
        bail!("required system program {} is not trusted", path.display());
    }
    for (index, node) in resolution.nodes.iter().enumerate() {
        let is_last = index + 1 == resolution.nodes.len();
        if !node.path.is_absolute() || node.uid != 0 {
            bail!("required system program {} is not trusted", path.display());
        }
        match node.kind {
            SystemNodeKind::Directory if !is_last => {
                if node.mode & 0o022 != 0 {
                    bail!("required system program {} is not trusted", path.display());
                }
            }
            // POSIX reports symlinks as mode 0777 on Linux and macOS. Their
            // ownership and trusted parent control replacement; their mode is
            // intentionally not treated as an access-control bitmask.
            SystemNodeKind::Symlink if !is_last => {
                if node.link_target.is_none() {
                    bail!("required system program {} is not trusted", path.display());
                }
            }
            SystemNodeKind::File if is_last => {
                if node.path != resolution.canonical_path
                    || node.mode & 0o111 == 0
                    || node.mode & 0o022 != 0
                {
                    bail!("required system program {} is not trusted", path.display());
                }
            }
            _ => bail!("required system program {} is not trusted", path.display()),
        }
    }
    Ok(())
}

fn resolve_system_program(requested: &Path) -> Result<SystemProgramResolution> {
    resolve_system_program_with(requested, &OsSystemPathAccess)
}

fn resolve_system_program_with(
    requested: &Path,
    access: &impl SystemPathAccess,
) -> Result<SystemProgramResolution> {
    let mut pending = path_components(requested, true)?;
    let mut resolved = PathBuf::from("/");
    let mut nodes = vec![access.node(Path::new("/"))?];
    let mut symlinks = 0_u8;

    while let Some(component) = pending.pop_front() {
        match component {
            ResolutionComponent::Parent => {
                // Absolute POSIX lookup clamps `..` at `/`; later components
                // are still checked, so an escape toward `/tmp` is rejected
                // when its writable ancestry is encountered.
                resolved.pop();
                if resolved.as_os_str().is_empty() {
                    resolved.push("/");
                }
            }
            ResolutionComponent::Normal(component) => {
                let candidate = resolved.join(component);
                let mut node = access.node(&candidate)?;
                if node.kind == SystemNodeKind::Symlink {
                    symlinks = symlinks.saturating_add(1);
                    if symlinks > 40 {
                        bail!("too many system program symlinks");
                    }
                    let target = access.read_link(&candidate)?;
                    if target.as_os_str().is_empty() {
                        bail!("system program symlink has an empty target");
                    }
                    node.link_target = Some(target.clone());
                    nodes.push(node);
                    let absolute = target.is_absolute();
                    let mut target_components = path_components(&target, absolute)?;
                    target_components.append(&mut pending);
                    pending = target_components;
                    if absolute {
                        resolved = PathBuf::from("/");
                    }
                } else {
                    resolved = candidate;
                    nodes.push(node);
                }
            }
        }
    }

    Ok(SystemProgramResolution {
        canonical_path: resolved,
        nodes,
    })
}

#[derive(Debug)]
enum ResolutionComponent {
    Parent,
    Normal(OsString),
}

fn path_components(path: &Path, require_absolute: bool) -> Result<VecDeque<ResolutionComponent>> {
    if require_absolute && !path.is_absolute() {
        bail!("system program path must be absolute");
    }
    let mut components = VecDeque::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => components.push_back(ResolutionComponent::Parent),
            std::path::Component::Normal(value) => {
                components.push_back(ResolutionComponent::Normal(value.to_os_string()));
            }
            std::path::Component::Prefix(_) => bail!("unsupported system program path prefix"),
        }
    }
    Ok(components)
}

fn system_node(path: &Path, link_target: Option<PathBuf>) -> Result<SystemNode> {
    let metadata = fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_dir() {
        SystemNodeKind::Directory
    } else if file_type.is_symlink() {
        SystemNodeKind::Symlink
    } else if file_type.is_file() {
        SystemNodeKind::File
    } else {
        SystemNodeKind::Other
    };
    Ok(SystemNode {
        path: path.to_path_buf(),
        kind,
        uid: metadata.uid(),
        mode: metadata.permissions().mode(),
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
        link_target,
    })
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
    reject_secure_owner_directory(path)
}

fn create_exclusive_temp_directory(
    candidates: impl IntoIterator<Item = PathBuf>,
) -> Result<PathBuf> {
    for path in candidates {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&path) {
            Ok(()) => {
                reject_secure_owner_directory(&path)?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("could not create CLI maintenance directory"),
        }
    }
    bail!("could not allocate a fresh CLI maintenance directory")
}

fn create_secure_empty_file(path: &Path) -> Result<File> {
    let file = open_new_empty_file(path)?;
    if let Err(error) = validate_new_empty_file(path, &file) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(file)
}

fn create_exclusive_temp_file(
    candidates: impl IntoIterator<Item = PathBuf>,
) -> Result<(PathBuf, File)> {
    for path in candidates {
        match open_new_empty_file(&path) {
            Ok(file) => {
                if let Err(error) = validate_new_empty_file(&path, &file) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                return Ok((path, file));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("could not create CLI maintenance file"),
        }
    }
    bail!("could not allocate a fresh CLI maintenance file")
}

fn open_new_empty_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

fn validate_new_empty_file(path: &Path, file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() != 0
        || metadata.nlink() != 1
    {
        bail!("CLI maintenance file {} is unsafe", path.display());
    }
    Ok(())
}

fn reject_secure_empty_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() != 0
        || metadata.nlink() != 1
    {
        bail!("CLI maintenance file {} is unsafe", path.display());
    }
    Ok(())
}

fn reject_secure_owner_directory(path: &Path) -> Result<()> {
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

    fn system_test_node(
        path: &str,
        kind: SystemNodeKind,
        uid: u32,
        mode: u32,
        inode: u64,
        link_target: Option<&str>,
    ) -> SystemNode {
        SystemNode {
            path: PathBuf::from(path),
            kind,
            uid,
            mode,
            device: 1,
            inode,
            size: 1,
            modified_seconds: 10,
            modified_nanoseconds: 20,
            changed_seconds: 30,
            changed_nanoseconds: 40,
            link_target: link_target.map(PathBuf::from),
        }
    }

    fn trusted_shell_resolution() -> SystemProgramResolution {
        SystemProgramResolution {
            canonical_path: PathBuf::from("/usr/bin/dash"),
            nodes: vec![
                system_test_node("/", SystemNodeKind::Directory, 0, 0o755, 1, None),
                system_test_node(
                    "/bin",
                    SystemNodeKind::Symlink,
                    0,
                    0o777,
                    2,
                    Some("usr/bin"),
                ),
                system_test_node("/usr", SystemNodeKind::Directory, 0, 0o755, 3, None),
                system_test_node("/usr/bin", SystemNodeKind::Directory, 0, 0o755, 4, None),
                system_test_node(
                    "/usr/bin/sh",
                    SystemNodeKind::Symlink,
                    0,
                    0o777,
                    5,
                    Some("dash"),
                ),
                system_test_node("/usr/bin/dash", SystemNodeKind::File, 0, 0o755, 6, None),
            ],
        }
    }

    fn direct_macos_shell_resolution() -> SystemProgramResolution {
        SystemProgramResolution {
            canonical_path: PathBuf::from("/bin/sh"),
            nodes: vec![
                system_test_node("/", SystemNodeKind::Directory, 0, 0o755, 1, None),
                system_test_node("/bin", SystemNodeKind::Directory, 0, 0o755, 2, None),
                system_test_node("/bin/sh", SystemNodeKind::File, 0, 0o755, 3, None),
            ],
        }
    }

    fn direct_curl_resolution() -> SystemProgramResolution {
        SystemProgramResolution {
            canonical_path: PathBuf::from("/usr/bin/curl"),
            nodes: vec![
                system_test_node("/", SystemNodeKind::Directory, 0, 0o755, 1, None),
                system_test_node("/usr", SystemNodeKind::Directory, 0, 0o755, 2, None),
                system_test_node("/usr/bin", SystemNodeKind::Directory, 0, 0o755, 3, None),
                system_test_node("/usr/bin/curl", SystemNodeKind::File, 0, 0o755, 4, None),
            ],
        }
    }

    struct SyntheticPathAccess {
        nodes: BTreeMap<PathBuf, SystemNode>,
    }

    impl SyntheticPathAccess {
        fn new(nodes: impl IntoIterator<Item = SystemNode>) -> Self {
            Self {
                nodes: nodes
                    .into_iter()
                    .map(|node| (node.path.clone(), node))
                    .collect(),
            }
        }
    }

    impl SystemPathAccess for SyntheticPathAccess {
        fn node(&self, path: &Path) -> Result<SystemNode> {
            let mut node = self
                .nodes
                .get(path)
                .cloned()
                .with_context(|| format!("injected path {} is missing", path.display()))?;
            node.link_target = None;
            Ok(node)
        }

        fn read_link(&self, path: &Path) -> Result<PathBuf> {
            self.nodes
                .get(path)
                .and_then(|node| node.link_target.clone())
                .with_context(|| format!("injected symlink {} is missing", path.display()))
        }
    }

    struct QueuedSystemResolver(Mutex<VecDeque<SystemProgramResolution>>);

    impl QueuedSystemResolver {
        fn stable(resolution: SystemProgramResolution) -> Self {
            Self(Mutex::new(VecDeque::from([resolution.clone(), resolution])))
        }
    }

    impl SystemProgramResolver for QueuedSystemResolver {
        fn resolve(&self, _requested: &Path) -> Result<SystemProgramResolution> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .context("injected system resolution was not configured")
        }
    }

    struct TestCurlHome(PathBuf);

    impl TestCurlHome {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "atmux-curl-home-test-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            create_exclusive_temp_directory([path.clone()]).unwrap();
            drop(create_secure_empty_file(&path.join(".curlrc")).unwrap());
            Self(path)
        }
    }

    impl Drop for TestCurlHome {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.0.join(".curlrc"));
            let _ = fs::remove_dir(&self.0);
        }
    }

    struct TestCodexInstall {
        home: PathBuf,
        release: String,
        outside: PathBuf,
    }

    impl TestCodexInstall {
        fn new(package_mode: u32) -> Self {
            let home = PathBuf::from("/tmp").join(format!(
                "atu-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&home).unwrap();
            fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
            for relative in [".local", ".local/bin", ".codex"] {
                let path = home.join(relative);
                fs::create_dir(&path).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let release = "r1".to_owned();
            let package_directories = [
                home.join(".codex/packages"),
                home.join(".codex/packages/standalone"),
                home.join(".codex/packages/standalone/releases"),
                home.join(".codex/packages/standalone/releases")
                    .join(&release),
                home.join(".codex/packages/standalone/releases")
                    .join(&release)
                    .join("bin"),
            ];
            for path in &package_directories {
                fs::create_dir(path).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(package_mode)).unwrap();
            }
            let executable = package_directories.last().unwrap().join("codex");
            fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink(
                home.join(".codex/packages/standalone/releases")
                    .join(&release),
                home.join(".codex/packages/standalone/current"),
            )
            .unwrap();
            std::os::unix::fs::symlink(
                home.join(".codex/packages/standalone/current/bin/codex"),
                home.join(".local/bin/codex"),
            )
            .unwrap();
            let outside = home.join("outside");
            fs::create_dir(&outside).unwrap();
            fs::set_permissions(&outside, fs::Permissions::from_mode(0o777)).unwrap();
            Self {
                home,
                release,
                outside,
            }
        }

        fn package_directories(&self) -> [PathBuf; 5] {
            let releases = self.home.join(".codex/packages/standalone/releases");
            [
                self.home.join(".codex/packages"),
                self.home.join(".codex/packages/standalone"),
                releases.clone(),
                releases.join(&self.release),
                releases.join(&self.release).join("bin"),
            ]
        }

        fn executable(&self) -> PathBuf {
            self.package_directories().last().unwrap().join("codex")
        }
    }

    impl Drop for TestCodexInstall {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    fn real_system_launchers_accept_linux_and_macos_layouts() {
        for requested in ["/bin/sh", "/usr/bin/curl"] {
            let trusted = trusted_system_program(Path::new(requested)).unwrap();
            assert_eq!(trusted, fs::canonicalize(requested).unwrap());
            assert!(trusted.is_absolute());
        }
    }

    #[test]
    fn codex_installer_download_is_fixed_and_deadline_bounded() {
        let arguments = codex_download_arguments("180", "/owner/state/codex-install-1");
        assert_eq!(
            arguments,
            [
                "--disable",
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--max-time",
                "180",
                "--output",
                "/owner/state/codex-install-1",
                "https://chatgpt.com/codex/install.sh",
            ]
        );
        assert_eq!(MaintenanceConfig::default().update_timeout_seconds, 180);
        assert_eq!(
            codex_installer_arguments("/owner/state/codex-install-1").unwrap(),
            [
                "-c",
                "umask 077 && exec /bin/sh \"$1\"",
                "atmux-codex-installer",
                "/owner/state/codex-install-1",
            ]
        );
        assert!(codex_installer_arguments("relative/install.sh").is_err());
        assert!(codex_installer_arguments("-installer").is_err());
        assert!(
            MaintenanceConfig {
                update_timeout_seconds: 901,
                ..MaintenanceConfig::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn fixed_installer_shell_applies_secure_umask_to_nested_directories_and_executable() {
        let root = std::env::temp_dir().join(format!(
            "atmux-installer-umask-test-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let script = root.join("installer.sh");
        fs::write(
            &script,
            b"mkdir -p \"$HOME/output/one/two\"\nprintf '#!/bin/sh\\nexit 0\\n' > \"$HOME/output/one/two/tool\"\nchmod +x \"$HOME/output/one/two/tool\"\n",
        )
        .unwrap();
        let script_text = script.to_str().unwrap();
        let arguments = codex_installer_arguments(script_text).unwrap();
        let status = std::process::Command::new("/bin/sh")
            .args(arguments)
            .env_clear()
            .env("HOME", &root)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .status()
            .unwrap();
        assert!(status.success());
        for path in [
            root.join("output"),
            root.join("output/one"),
            root.join("output/one/two"),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let tool = root.join("output/one/two/tool");
        assert_eq!(
            fs::metadata(&tool).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            std::process::Command::new(&tool)
                .status()
                .unwrap()
                .success()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preexisting_permissive_codex_package_tree_recovers_before_vendor_update() {
        let fixture = TestCodexInstall::new(0o775);
        fs::set_permissions(
            fixture.home.join(".codex"),
            fs::Permissions::from_mode(0o711),
        )
        .unwrap();
        fs::set_permissions(
            fixture.home.join(".local"),
            fs::Permissions::from_mode(0o711),
        )
        .unwrap();
        assert!(resolve_launcher_at(&fixture.home, Harness::Codex).is_none());

        let recovered = resolve_launcher_with_recovery_at(&fixture.home, Harness::Codex)
            .unwrap()
            .unwrap();
        assert_eq!(recovered, fixture.executable());
        for path in fixture.package_directories() {
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o022, 0);
        }
        assert_eq!(
            fs::metadata(fixture.home.join(".codex"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o711
        );
        assert_eq!(
            fs::metadata(fixture.home.join(".local"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o711
        );
        assert_eq!(
            fs::metadata(&fixture.outside).unwrap().permissions().mode() & 0o777,
            0o777
        );
        assert!(
            std::process::Command::new(recovered)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn package_repair_preserves_special_directory_bits_while_clearing_write() {
        let fixture = TestCodexInstall::new(0o775);
        let releases = fixture.home.join(".codex/packages/standalone/releases");
        fs::set_permissions(&releases, fs::Permissions::from_mode(0o3775)).unwrap();
        reconcile_codex_package_permissions(&fixture.home).unwrap();
        assert_eq!(
            fs::metadata(releases).unwrap().permissions().mode() & 0o7777,
            0o3755
        );
    }

    #[test]
    fn package_mode_conversion_is_checked_before_platform_raw_mode() {
        assert_eq!(checked_mode::<u16>(0o3755), Some(0o3755));
        assert_eq!(checked_mode::<u16>(u32::from(u16::MAX) + 1), None);

        let raw = checked_raw_mode(0o3755).unwrap();
        assert_eq!(raw, 0o3755);
    }

    #[test]
    fn package_repair_rejects_non_fixed_release_shape_without_touching_outside() {
        let fixture = TestCodexInstall::new(0o775);
        let current = fixture.home.join(".codex/packages/standalone/current");
        fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink(
            fixture
                .home
                .join(".codex/packages/standalone/releases")
                .join(&fixture.release)
                .join("extra"),
            current,
        )
        .unwrap();
        assert!(reconcile_codex_package_permissions(&fixture.home).is_err());
        assert_eq!(
            fs::metadata(&fixture.outside).unwrap().permissions().mode() & 0o777,
            0o777
        );
    }

    #[test]
    fn package_repair_detects_directory_symlink_swap_and_never_chmods_target() {
        let fixture = TestCodexInstall::new(0o775);
        let packages = fixture.home.join(".codex/packages");
        let displaced = fixture.home.join(".codex/packages-displaced");
        let outside = fixture.outside.clone();
        let mut swapped = false;
        let result = reconcile_codex_package_permissions_with(&fixture.home, |stage, path| {
            if !swapped && stage == CodexPermissionStage::DirectoryOpened && path == packages {
                fs::rename(&packages, &displaced)?;
                std::os::unix::fs::symlink(&outside, &packages)?;
                swapped = true;
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(
            fs::metadata(outside).unwrap().permissions().mode() & 0o777,
            0o777
        );
    }

    #[test]
    fn package_repair_detects_public_launcher_parent_swap() {
        let fixture = TestCodexInstall::new(0o775);
        let bin = fixture.home.join(".local/bin");
        let displaced = fixture.home.join(".local/bin-displaced");
        let expected_target = fixture
            .home
            .join(".codex/packages/standalone/current/bin/codex");
        let mut swapped = false;
        let result = reconcile_codex_package_permissions_with(&fixture.home, |stage, path| {
            if !swapped && stage == CodexPermissionStage::DirectoryOpened && path == bin {
                fs::rename(&bin, &displaced)?;
                fs::create_dir(&bin)?;
                fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))?;
                std::os::unix::fs::symlink(&expected_target, bin.join("codex"))?;
                swapped = true;
            }
            Ok(())
        });
        assert!(result.is_err());
    }

    #[test]
    fn package_repair_detects_target_swap_after_descriptor_open() {
        let fixture = TestCodexInstall::new(0o775);
        let target = fixture.executable();
        let displaced = target.with_extension("old");
        let mut swapped = false;
        let result = reconcile_codex_package_permissions_with(&fixture.home, |stage, path| {
            if !swapped && stage == CodexPermissionStage::TargetOpened && path == target {
                fs::rename(&target, &displaced)?;
                fs::write(&target, b"#!/bin/sh\nexit 0\n")?;
                fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
                swapped = true;
            }
            Ok(())
        });
        assert!(result.is_err());
    }

    #[test]
    fn package_repair_rejects_hardlinked_special_and_wrong_owner_targets() {
        let hardlinked = TestCodexInstall::new(0o775);
        let second_link = hardlinked.executable().with_extension("hardlink");
        fs::hard_link(hardlinked.executable(), second_link).unwrap();
        assert!(reconcile_codex_package_permissions(&hardlinked.home).is_err());

        let special = TestCodexInstall::new(0o775);
        let target = special.executable();
        fs::remove_file(&target).unwrap();
        std::os::unix::net::UnixListener::bind(&target).unwrap();
        assert!(reconcile_codex_package_permissions(&special.home).is_err());

        let wrong_owner = TestCodexInstall::new(0o775);
        let target = wrong_owner.executable();
        let file = File::open(&target).unwrap();
        assert!(
            validate_owner_executable(
                &file,
                &target,
                rustix::process::geteuid().as_raw().wrapping_add(1),
            )
            .is_err()
        );
    }

    #[test]
    fn package_repair_rejects_hardlinked_launcher_symlink() {
        let fixture = TestCodexInstall::new(0o775);
        let launcher = fixture.home.join(".local/bin/codex");
        fs::hard_link(&launcher, fixture.home.join(".local/bin/codex-second-link")).unwrap();
        assert!(!reconcile_codex_package_permissions(&fixture.home).unwrap());
        assert_eq!(
            fs::metadata(fixture.home.join(".codex/packages"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o775
        );
    }

    #[test]
    fn root_owned_0777_symlinks_are_trusted_but_user_paths_are_not() {
        let resolver = QueuedSystemResolver::stable(trusted_shell_resolution());
        assert_eq!(
            trusted_system_program_with(Path::new("/bin/sh"), &resolver).unwrap(),
            Path::new("/usr/bin/dash")
        );

        let unused = QueuedSystemResolver(Mutex::new(VecDeque::new()));
        assert!(
            trusted_system_program_with(Path::new("/tmp/curl"), &unused)
                .unwrap_err()
                .to_string()
                .contains("not allow-listed")
        );
    }

    #[test]
    fn synthetic_linux_merged_usr_and_direct_macos_layouts_resolve() {
        let linux_expected = trusted_shell_resolution();
        let linux_access = SyntheticPathAccess::new(linux_expected.nodes.clone());
        let linux = resolve_system_program_with(Path::new("/bin/sh"), &linux_access).unwrap();
        assert_eq!(linux, linux_expected);
        validate_system_resolution(Path::new("/bin/sh"), &linux).unwrap();

        let macos_expected = direct_macos_shell_resolution();
        let macos_access = SyntheticPathAccess::new(macos_expected.nodes.clone());
        let macos = resolve_system_program_with(Path::new("/bin/sh"), &macos_access).unwrap();
        assert_eq!(macos, macos_expected);
        validate_system_resolution(Path::new("/bin/sh"), &macos).unwrap();
    }

    #[test]
    fn each_fixed_spawn_is_revalidated_immediately_before_command_construction() {
        let curl_home = TestCurlHome::new();
        let shell_resolver = QueuedSystemResolver::stable(direct_macos_shell_resolution());
        let installer_arguments = codex_installer_arguments("/owner/state/install").unwrap();
        let (shell, shell_command) = validated_system_command_with(
            Path::new("/bin/sh"),
            &installer_arguments,
            &shell_resolver,
            &curl_home.0,
        )
        .unwrap();
        assert_eq!(shell, Path::new("/bin/sh"));
        assert_eq!(shell_command.as_std().get_program(), Path::new("/bin/sh"));
        assert_eq!(
            shell_command.as_std().get_args().collect::<Vec<_>>(),
            installer_arguments
                .iter()
                .map(OsStr::new)
                .collect::<Vec<_>>()
        );
        assert!(shell_resolver.0.lock().unwrap().is_empty());

        let curl_resolver = QueuedSystemResolver::stable(direct_curl_resolution());
        let arguments = codex_download_arguments("180", "/owner/state/install");
        let (curl, curl_command) = validated_system_command_with(
            Path::new("/usr/bin/curl"),
            &arguments,
            &curl_resolver,
            &curl_home.0,
        )
        .unwrap();
        assert_eq!(curl, Path::new("/usr/bin/curl"));
        assert_eq!(
            curl_command.as_std().get_program(),
            Path::new("/usr/bin/curl")
        );
        assert_eq!(
            curl_command.as_std().get_args().next(),
            Some(std::ffi::OsStr::new("--disable"))
        );
        assert!(curl_resolver.0.lock().unwrap().is_empty());

        let environment: BTreeMap<_, _> = curl_command.as_std().get_envs().collect();
        assert_eq!(environment.len(), 4);
        assert_eq!(
            environment.get(std::ffi::OsStr::new("PATH")),
            Some(&Some(std::ffi::OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin")))
        );
        assert!(environment.contains_key(std::ffi::OsStr::new("HOME")));
        assert_eq!(
            environment.get(std::ffi::OsStr::new("CURL_HOME")),
            Some(&Some(curl_home.0.as_os_str()))
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("CODEX_NON_INTERACTIVE")),
            Some(&Some(std::ffi::OsStr::new("1")))
        );
        assert_eq!(fs::metadata(curl_home.0.join(".curlrc")).unwrap().len(), 0);

        let shell_environment: BTreeMap<_, _> = shell_command.as_std().get_envs().collect();
        assert_eq!(shell_environment, environment);

        fs::write(curl_home.0.join(".curlrc"), b"proxy = hostile.invalid\n").unwrap();
        assert!(
            validated_system_command_with(
                Path::new("/bin/sh"),
                &["/owner/state/install"],
                &QueuedSystemResolver::stable(direct_macos_shell_resolution()),
                &curl_home.0,
            )
            .is_err(),
            "a config changed before spawn must fail closed"
        );
    }

    #[test]
    fn stale_curl_home_collision_is_never_reused() {
        let root = std::env::temp_dir().join(format!(
            "atmux-curl-collision-test-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        secure_owner_directory(&root).unwrap();
        let stale = root.join("stale");
        let fresh = root.join("fresh");
        create_exclusive_temp_directory([stale.clone()]).unwrap();
        fs::write(stale.join(".curlrc"), b"proxy = hostile.invalid\n").unwrap();

        let selected = create_exclusive_temp_directory([stale.clone(), fresh.clone()]).unwrap();
        assert_eq!(selected, fresh);
        assert_eq!(
            fs::read(stale.join(".curlrc")).unwrap(),
            b"proxy = hostile.invalid\n"
        );
        let config = selected.join(".curlrc");
        drop(create_secure_empty_file(&config).unwrap());
        assert_eq!(fs::metadata(&config).unwrap().len(), 0);

        let _ = fs::remove_file(config);
        let _ = fs::remove_dir(selected);
        let _ = fs::remove_file(stale.join(".curlrc"));
        let _ = fs::remove_dir(stale);
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn stale_script_collision_retries_and_partial_cleanup_is_reverse_ordered() {
        let root = std::env::temp_dir().join(format!(
            "atmux-script-collision-test-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        secure_owner_directory(&root).unwrap();
        let stale = root.join("stale-script");
        let fresh = root.join("fresh-script");
        fs::write(&stale, b"stale installer bytes").unwrap();
        let (selected, file) = create_exclusive_temp_file([stale.clone(), fresh.clone()]).unwrap();
        assert_eq!(selected, fresh);
        assert_eq!(file.metadata().unwrap().len(), 0);
        assert_eq!(file.metadata().unwrap().nlink(), 1);
        drop(file);
        assert_eq!(fs::read(&stale).unwrap(), b"stale installer bytes");

        let partial = root.join("partial");
        create_exclusive_temp_directory([partial.clone()]).unwrap();
        let partial_file = partial.join(".curlrc");
        drop(create_secure_empty_file(&partial_file).unwrap());
        {
            let mut cleanup = TempCleanup::default();
            cleanup.directory(&partial);
            cleanup.file(&partial_file);
        }
        assert!(!partial_file.exists());
        assert!(!partial.exists());

        let _ = fs::remove_file(selected);
        let _ = fs::remove_file(stale);
        let _ = fs::remove_dir(root);
    }

    #[test]
    fn owner_home_requires_a_non_writable_root_or_owner_ancestor_chain() {
        let euid = 501;
        let trusted = vec![
            system_test_node("/", SystemNodeKind::Directory, 0, 0o755, 1, None),
            system_test_node("/Users", SystemNodeKind::Directory, 0, 0o755, 2, None),
            system_test_node(
                "/Users/ryan",
                SystemNodeKind::Directory,
                euid,
                0o700,
                3,
                None,
            ),
        ];
        assert!(owner_home_nodes_trusted(&trusted, euid));

        let mut writable_ancestor = trusted.clone();
        writable_ancestor[1].mode = 0o777;
        assert!(!owner_home_nodes_trusted(&writable_ancestor, euid));

        let mut foreign_ancestor = trusted.clone();
        foreign_ancestor[1].uid = 502;
        assert!(!owner_home_nodes_trusted(&foreign_ancestor, euid));

        let mut root_owned_terminal = trusted;
        root_owned_terminal.last_mut().unwrap().uid = 0;
        assert!(!owner_home_nodes_trusted(&root_owned_terminal, euid));
    }

    #[test]
    fn writable_ancestor_target_and_unowned_symlink_are_rejected() {
        let mut writable_parent = trusted_shell_resolution();
        writable_parent.nodes[3].mode = 0o775;
        assert!(
            trusted_system_program_with(
                Path::new("/bin/sh"),
                &QueuedSystemResolver::stable(writable_parent)
            )
            .is_err()
        );

        let mut writable_target = trusted_shell_resolution();
        writable_target.nodes.last_mut().unwrap().mode = 0o775;
        assert!(
            trusted_system_program_with(
                Path::new("/bin/sh"),
                &QueuedSystemResolver::stable(writable_target)
            )
            .is_err()
        );

        let mut unowned_symlink = trusted_shell_resolution();
        unowned_symlink.nodes[1].uid = 501;
        assert!(
            trusted_system_program_with(
                Path::new("/bin/sh"),
                &QueuedSystemResolver::stable(unowned_symlink)
            )
            .is_err()
        );
    }

    #[test]
    fn relative_escape_into_writable_namespace_is_rejected() {
        let mut escaped = trusted_shell_resolution();
        escaped.canonical_path = PathBuf::from("/tmp/sh");
        escaped.nodes = vec![
            system_test_node("/", SystemNodeKind::Directory, 0, 0o755, 1, None),
            system_test_node("/bin", SystemNodeKind::Directory, 0, 0o755, 2, None),
            system_test_node(
                "/bin/sh",
                SystemNodeKind::Symlink,
                0,
                0o777,
                3,
                Some("../../tmp/sh"),
            ),
            system_test_node("/tmp", SystemNodeKind::Directory, 0, 0o1777, 4, None),
            system_test_node("/tmp/sh", SystemNodeKind::File, 0, 0o755, 5, None),
        ];
        assert!(
            trusted_system_program_with(
                Path::new("/bin/sh"),
                &QueuedSystemResolver::stable(escaped)
            )
            .is_err()
        );
    }

    #[test]
    fn symlink_loop_and_more_than_forty_links_are_rejected() {
        let loop_access = SyntheticPathAccess::new([
            system_test_node("/", SystemNodeKind::Directory, 0, 0o755, 1, None),
            system_test_node("/bin", SystemNodeKind::Directory, 0, 0o755, 2, None),
            system_test_node("/bin/sh", SystemNodeKind::Symlink, 0, 0o777, 3, Some("sh")),
        ]);
        assert!(
            resolve_system_program_with(Path::new("/bin/sh"), &loop_access)
                .unwrap_err()
                .to_string()
                .contains("too many")
        );

        let mut deep_nodes = vec![
            system_test_node("/", SystemNodeKind::Directory, 0, 0o755, 1, None),
            system_test_node("/bin", SystemNodeKind::Directory, 0, 0o755, 2, None),
            system_test_node(
                "/bin/sh",
                SystemNodeKind::Symlink,
                0,
                0o777,
                3,
                Some("link-0"),
            ),
        ];
        for index in 0_u64..40 {
            deep_nodes.push(system_test_node(
                &format!("/bin/link-{index}"),
                SystemNodeKind::Symlink,
                0,
                0o777,
                index + 4,
                Some(&format!("link-{}", index + 1)),
            ));
        }
        let deep_access = SyntheticPathAccess::new(deep_nodes);
        assert!(
            resolve_system_program_with(Path::new("/bin/sh"), &deep_access)
                .unwrap_err()
                .to_string()
                .contains("too many")
        );
    }

    #[test]
    fn terminal_non_regular_non_executable_and_non_root_nodes_are_rejected() {
        for mutate in [
            |node: &mut SystemNode| node.kind = SystemNodeKind::Other,
            |node: &mut SystemNode| node.mode = 0o644,
            |node: &mut SystemNode| node.uid = 501,
        ] {
            let mut resolution = direct_macos_shell_resolution();
            mutate(resolution.nodes.last_mut().unwrap());
            assert!(validate_system_resolution(Path::new("/bin/sh"), &resolution).is_err());
        }
    }

    #[test]
    fn metadata_and_readlink_target_swaps_between_validation_passes_are_rejected() {
        let before = trusted_shell_resolution();
        let mut after = before.clone();
        after.nodes.last_mut().unwrap().inode += 1;
        let resolver = QueuedSystemResolver(Mutex::new(VecDeque::from([before, after])));
        assert!(
            trusted_system_program_with(Path::new("/bin/sh"), &resolver)
                .unwrap_err()
                .to_string()
                .contains("changed during validation")
        );

        let before = trusted_shell_resolution();
        let mut after = before.clone();
        after.nodes[4].link_target = Some(PathBuf::from("bash"));
        after.nodes.last_mut().unwrap().path = PathBuf::from("/usr/bin/bash");
        after.canonical_path = PathBuf::from("/usr/bin/bash");
        let resolver = QueuedSystemResolver(Mutex::new(VecDeque::from([before, after])));
        assert!(
            trusted_system_program_with(Path::new("/bin/sh"), &resolver)
                .unwrap_err()
                .to_string()
                .contains("changed during validation")
        );
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
