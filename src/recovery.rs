//! Host-scoped restart recovery.
//!
//! This deliberately exposes one fixed operation rather than a generic command
//! runner.  Only Tron's canonical, locally owned recovery script is eligible,
//! browser callers cannot supply a path or arguments, output is never returned,
//! and one process may run at a time.

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

const TRON_MACHINE_ID: &str = "tron";
const TRON_RESUME_SCRIPT: &str = "/home/ryan/resume-tron.sh";
const SCRIPT_MARKER: &str = "ATMUX_QUICK_RESUME_IDEMPOTENT_V1";
const MAX_SCRIPT_BYTES: u64 = 1024 * 1024;
const RUN_TIMEOUT: Duration = Duration::from_secs(180);
#[cfg(not(test))]
const PROCESS_GROUP_GRACE: Duration = Duration::from_secs(5);
#[cfg(test)]
const PROCESS_GROUP_GRACE: Duration = Duration::from_millis(100);
const BASH_COMMAND: &str = "/usr/bin/bash";
const RECOVERY_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/snap/bin:/home/ryan/.asdf/shims:/home/ryan/.local/bin";
const TRON_HOME: &str = "/home/ryan";
const REQUIRED_RECOVERY_COMMANDS: &[&str] = &[
    "/usr/bin/bash",
    "/usr/bin/tmux",
    "/home/ryan/.asdf/shims/codex",
    "/home/ryan/.local/bin/claude",
    "/home/ryan/.local/bin/claude-hd",
    "/home/ryan/.local/bin/claude-max",
];
const LOCK_FILE_NAME: &str = "resume-tron.lock";

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
            Self::Running => formatter.write_str("Tron recovery is already running"),
        }
    }
}

impl std::error::Error for RecoveryStartError {}

#[derive(Debug)]
struct RecoveryInner {
    enabled: bool,
    script: PathBuf,
    runtime_dir: PathBuf,
    required_commands: &'static [&'static str],
    timeout: Duration,
    state: Mutex<RecoveryStatus>,
}

#[derive(Debug)]
struct ValidatedScript {
    contents: Vec<u8>,
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
    #[must_use]
    pub fn production(machine: &str) -> Self {
        let uid = geteuid().as_raw();
        Self::new(
            machine,
            machine == TRON_MACHINE_ID,
            PathBuf::from(TRON_RESUME_SCRIPT),
            PathBuf::from(format!("/run/user/{uid}/atmux")),
            RUN_TIMEOUT,
            REQUIRED_RECOVERY_COMMANDS,
        )
    }

    fn new(
        machine: &str,
        enabled: bool,
        script: PathBuf,
        runtime_dir: PathBuf,
        timeout: Duration,
        required_commands: &'static [&'static str],
    ) -> Self {
        let available = enabled
            && validate_script(&script).is_ok()
            && validate_runtime_location(&runtime_dir).is_ok()
            && required_commands_available(required_commands);
        let (phase, message) = if !enabled {
            (
                RecoveryPhase::Unavailable,
                "Quick Resume is available only on Tron".to_owned(),
            )
        } else if available {
            (
                RecoveryPhase::Idle,
                "Ready to restore Tron's saved session roster".to_owned(),
            )
        } else {
            (
                RecoveryPhase::Unavailable,
                "Tron's recovery script is unavailable or fails its safety checks".to_owned(),
            )
        };
        Self {
            inner: Arc::new(RecoveryInner {
                enabled,
                script,
                runtime_dir,
                timeout,
                required_commands,
                state: Mutex::new(RecoveryStatus {
                    machine: machine.to_owned(),
                    available,
                    phase,
                    started_at_ms: None,
                    finished_at_ms: None,
                    message,
                }),
            }),
        }
    }

    pub async fn status(&self) -> RecoveryStatus {
        let mut state = self.inner.state.lock().await;
        if state.phase != RecoveryPhase::Running {
            state.available = self.inner.enabled
                && validate_script(&self.inner.script).is_ok()
                && validate_runtime_location(&self.inner.runtime_dir).is_ok()
                && required_commands_available(self.inner.required_commands);
            if !state.available {
                state.phase = RecoveryPhase::Unavailable;
                state.message = if self.inner.enabled {
                    "Tron's recovery script is unavailable or fails its safety checks".to_owned()
                } else {
                    "Quick Resume is available only on Tron".to_owned()
                };
            } else if state.phase == RecoveryPhase::Unavailable {
                state.phase = RecoveryPhase::Idle;
                "Ready to restore Tron's saved session roster".clone_into(&mut state.message);
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
    /// fails validation or recovery is disabled for this machine.
    pub async fn start(&self) -> Result<RecoveryStatus, RecoveryStartError> {
        let mut state = self.inner.state.lock().await;
        if state.phase == RecoveryPhase::Running {
            return Err(RecoveryStartError::Running);
        }
        let script =
            if self.inner.enabled && required_commands_available(self.inner.required_commands) {
                validate_script(&self.inner.script).ok()
            } else {
                None
            };
        let Some(script) = script else {
            state.available = false;
            state.phase = RecoveryPhase::Unavailable;
            "Tron's recovery script is unavailable or fails its safety checks"
                .clone_into(&mut state.message);
            return Err(RecoveryStartError::Unavailable(state.message.clone()));
        };
        let lock = match acquire_runtime_lock(&self.inner.runtime_dir) {
            Ok(lock) => lock,
            Err(LockError::Busy) => return Err(RecoveryStartError::Running),
            Err(LockError::Unsafe) => {
                state.available = false;
                state.phase = RecoveryPhase::Unavailable;
                "Tron's secure runtime lock directory is unavailable"
                    .clone_into(&mut state.message);
                return Err(RecoveryStartError::Unavailable(state.message.clone()));
            }
        };

        let started_at_ms = now_ms();
        state.available = true;
        state.phase = RecoveryPhase::Running;
        state.started_at_ms = Some(started_at_ms);
        state.finished_at_ms = None;
        "Restoring missing Tron sessions; existing sessions are preserved"
            .clone_into(&mut state.message);
        let started = state.clone();
        drop(state);

        let runner = self.clone();
        tokio::spawn(async move {
            let outcome = run_script(script, lock, runner.inner.timeout).await;
            let mut state = runner.inner.state.lock().await;
            state.finished_at_ms = Some(now_ms());
            match outcome {
                ScriptOutcome::Succeeded => {
                    state.phase = RecoveryPhase::Succeeded;
                    "Tron recovery script finished; sessions will appear as they become ready"
                        .clone_into(&mut state.message);
                }
                ScriptOutcome::Failed(code) => {
                    state.phase = RecoveryPhase::Failed;
                    state.message = code.map_or_else(
                        || "Tron recovery script was terminated".to_owned(),
                        |code| format!("Tron recovery script exited with status {code}"),
                    );
                }
                ScriptOutcome::TimedOut => {
                    state.phase = RecoveryPhase::TimedOut;
                    "Tron recovery stopped after its three-minute safety limit"
                        .clone_into(&mut state.message);
                }
            }
        });
        Ok(started)
    }

    #[cfg(test)]
    fn fixture(machine: &str, script: PathBuf, timeout: Duration) -> Self {
        let runtime_dir = script.parent().unwrap().join("runtime");
        Self::new(machine, true, script, runtime_dir, timeout, &[])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScriptOutcome {
    Succeeded,
    Failed(Option<i32>),
    TimedOut,
}

async fn run_script(script: ValidatedScript, _lock: File, timeout: Duration) -> ScriptOutcome {
    // Execute the already-opened, validated bytes instead of reopening a path
    // after validation. The child leads a new process group so timeout cleanup
    // reaches every descendant the recovery script started.
    let mut command = Command::new(BASH_COMMAND);
    configure_script_environment(&mut command);
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

fn configure_script_environment(command: &mut Command) {
    // Bash evaluates BASH_ENV before stdin and imports exported shell
    // functions. Start from an empty environment so the pinned script bytes
    // are the only shell program that can execute, then add only fixed data the
    // Tron roster and its verification commands require.
    command.env_clear().envs([
        ("HOME", TRON_HOME),
        ("LANG", "C.UTF-8"),
        ("LOGNAME", "ryan"),
        ("PATH", RECOVERY_PATH),
        ("USER", "ryan"),
    ]);
}

fn required_commands_available(commands: &[&str]) -> bool {
    commands.iter().all(|command| {
        let path = Path::new(command);
        path.is_absolute()
            && fs::metadata(path)
                .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
    })
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

fn validate_script(path: &Path) -> Result<ValidatedScript, ()> {
    let euid = geteuid().as_raw();
    validate_secure_ancestry(path, euid)?;
    let path_metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| ())?;
    let mut file = File::from(descriptor);
    let metadata = file.metadata().map_err(|_| ())?;
    if !metadata.file_type().is_file()
        || metadata.len() > MAX_SCRIPT_BYTES
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
        || metadata.uid() != euid
        || metadata.nlink() != 1
        || path_metadata.dev() != metadata.dev()
        || path_metadata.ino() != metadata.ino()
    {
        return Err(());
    }
    let mut contents = Vec::with_capacity(usize::try_from(metadata.len()).map_err(|_| ())?);
    file.read_to_end(&mut contents).map_err(|_| ())?;
    if !contents
        .split(|byte| *byte == b'\n')
        .any(|line| line == format!("# {SCRIPT_MARKER}").as_bytes())
    {
        return Err(());
    }
    Ok(ValidatedScript { contents })
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

    fn fixture_script(body: &str) -> PathBuf {
        let directory = std::env::current_dir().unwrap().join(format!(
            ".atmux-recovery-test-{}-{}",
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("resume.sh");
        let mut file = File::create(&path).unwrap();
        writeln!(file, "#!/usr/bin/env bash\n# {SCRIPT_MARKER}\n{body}").unwrap();
        let mut permissions = file.metadata().unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).unwrap();
        path
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
        let runner = RecoveryRunner::fixture("tron", path.clone(), Duration::from_secs(2));
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
        let first = RecoveryRunner::fixture("tron", path.clone(), Duration::from_secs(2));
        let second = RecoveryRunner::fixture("tron", path.clone(), Duration::from_secs(2));
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
        let runner = RecoveryRunner::fixture("tron", path.clone(), Duration::from_secs(2));
        runner.start().await.unwrap();
        let status = wait_until_finished(&runner).await;
        assert_eq!(status.phase, RecoveryPhase::Failed);
        assert_eq!(status.message, "Tron recovery script exited with status 7");
        assert!(!serde_json::to_string(&status).unwrap().contains("secret"));
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
        configure_script_environment(&mut command);
        let status = command.arg("-c").arg(":").status().await.unwrap();
        assert!(status.success());
        assert!(!marker.exists(), "BASH_ENV code ran before the fixture");
        remove_fixture(&path);
    }

    #[test]
    fn sanitized_path_covers_every_preflighted_roster_command() {
        assert!(required_commands_available(&[BASH_COMMAND]));
        assert!(!required_commands_available(&[
            "/definitely/missing/atmux-recovery-command"
        ]));
        let path_entries = RECOVERY_PATH.split(':').collect::<Vec<_>>();
        for command in REQUIRED_RECOVERY_COMMANDS {
            let parent = Path::new(command).parent().unwrap().to_string_lossy();
            assert!(
                path_entries.contains(&parent.as_ref()),
                "sanitized PATH omits required command directory {parent}"
            );
        }
    }

    #[tokio::test]
    async fn timeout_stops_a_hung_fixture() {
        let path = fixture_script("sleep 30");
        let runner = RecoveryRunner::fixture("tron", path.clone(), Duration::from_secs(1));
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
        fs::write(
            &path,
            format!(
                "#!/usr/bin/env bash\n# {SCRIPT_MARKER}\n(trap '' TERM; while :; do sleep 30; done) &\nprintf '%s' \"$!\" > {}\nwait\n",
                shell_words::quote(&pid_file.display().to_string()),
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let runner = RecoveryRunner::fixture("tron", path.clone(), Duration::from_secs(1));
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
    async fn safety_marker_and_machine_scope_fail_closed() {
        let path = fixture_script(":");
        let wrong_machine = RecoveryRunner::production("midnight");
        assert!(!wrong_machine.status().await.available);

        fs::write(&path, "#!/usr/bin/env bash\nexit 0\n").unwrap();
        let runner = RecoveryRunner::fixture("tron", path.clone(), Duration::from_secs(1));
        assert!(!runner.status().await.available);
        assert!(matches!(
            runner.start().await,
            Err(RecoveryStartError::Unavailable(_))
        ));
        remove_fixture(&path);
    }

    #[tokio::test]
    async fn writable_or_symlinked_script_ancestry_fails_closed() {
        let path = fixture_script(":");
        let directory = path.parent().unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o770)).unwrap();
        let runner = RecoveryRunner::fixture("tron", path.clone(), Duration::from_secs(1));
        assert!(!runner.status().await.available);
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();

        let link = directory.join("linked.sh");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let linked = RecoveryRunner::fixture("tron", link, Duration::from_secs(1));
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

        let runner = RecoveryRunner::fixture("tron", path.clone(), Duration::from_secs(1));
        assert!(!runner.status().await.available);
        assert!(matches!(
            runner.start().await,
            Err(RecoveryStartError::Unavailable(_))
        ));
        remove_fixture(&path);
    }
}
