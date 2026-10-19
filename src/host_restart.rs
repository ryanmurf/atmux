//! Host-scoped machine restart.
//!
//! Like Quick Resume, this exposes one fixed operation per machine rather than
//! a command runner.  The reboot command comes from the owning node's own
//! configuration (or a platform default), browser callers cannot supply a
//! path or an argument, output is never returned, and one attempt runs at a
//! time.  A coordinator only forwards the request to the owning node.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rustix::process::Pid;
use serde::{Deserialize, Serialize};
use tokio::{process::Command, sync::Mutex};

use crate::{
    config::Config,
    recovery::{HostEnvironment, configure_script_environment, terminate_process_group},
};

/// How long the reboot command may take to hand the restart to the system.
/// `systemctl reboot` and `osascript` return in well under a second; a command
/// still running after this is stuck on a prompt it can never answer.
#[cfg(not(test))]
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const RUN_TIMEOUT: Duration = Duration::from_millis(400);

const READY_MESSAGE: &str = "Ready to restart this machine";
const DISABLED_MESSAGE: &str = "Restart is turned off in this machine's [restart] configuration";
const NO_IDENTITY_MESSAGE: &str = "Restart cannot determine this machine's user identity";
const RUNNING_MESSAGE: &str = "Asking the system to restart";
const REQUESTED_MESSAGE: &str =
    "Restart requested; this machine goes offline while it reboots and returns on its own";

/// The reboot command a node runs when its configuration names none.
///
/// Linux asks logind through `sudo -n`, so the atmux user needs a passwordless
/// sudoers entry for it.  macOS asks the login session itself, which needs no
/// root but does need Automation permission for System Events the first time.
#[must_use]
pub fn default_command() -> Vec<String> {
    if cfg!(target_os = "macos") {
        vec![
            "/usr/bin/osascript".to_owned(),
            "-e".to_owned(),
            "tell application \"System Events\" to restart".to_owned(),
        ]
    } else {
        vec![
            "/usr/bin/sudo".to_owned(),
            "-n".to_owned(),
            "/usr/bin/systemctl".to_owned(),
            "reboot".to_owned(),
        ]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostRestartPhase {
    /// The command is disabled, missing, or not executable.
    Unavailable,
    #[default]
    Idle,
    /// The reboot command is running.
    Restarting,
    /// The command succeeded, so the system has accepted the restart.
    Requested,
    /// The command failed or timed out; another attempt is allowed.
    Failed,
}

/// Safe restart state for a browser or a federated peer: a phase and a
/// message, never the command line or its output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRestartStatus {
    pub machine: String,
    pub available: bool,
    pub phase: HostRestartPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostRestartStartError {
    /// A reboot command is already running.
    Running,
    /// The fixed command cannot run; the string is the user-facing reason.
    Unavailable(String),
}

impl std::fmt::Display for HostRestartStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => f.write_str("a restart is already in progress on this machine"),
            Self::Unavailable(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for HostRestartStartError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandOutcome {
    Succeeded,
    Failed(Option<i32>),
    TimedOut,
}

#[derive(Debug)]
struct Inner {
    enabled: bool,
    command: Vec<String>,
    environment: Option<HostEnvironment>,
    timeout: Duration,
    state: Mutex<HostRestartStatus>,
}

impl Inner {
    /// The command to run, or the user-facing reason none can run.
    fn probe(&self) -> Result<Vec<String>, String> {
        if !self.enabled {
            return Err(DISABLED_MESSAGE.to_owned());
        }
        let Some(program) = self.command.first() else {
            return Err("No restart command is configured on this machine".to_owned());
        };
        if !is_executable_file(Path::new(program)) {
            return Err(format!(
                "Restart command {program} is missing or not executable on this machine"
            ));
        }
        Ok(self.command.clone())
    }
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Cloneable single-flight handle for one owning node.
#[derive(Clone, Debug)]
pub struct HostRestartRunner {
    inner: Arc<Inner>,
}

impl HostRestartRunner {
    /// Builds this node's runner from its configuration.
    #[must_use]
    pub fn production(config: &Config) -> Self {
        Self::new(
            &config.node.id,
            config.restart.enabled,
            config
                .restart
                .command
                .clone()
                .unwrap_or_else(default_command),
            HostEnvironment::capture(),
            RUN_TIMEOUT,
        )
    }

    fn new(
        machine: &str,
        enabled: bool,
        command: Vec<String>,
        environment: Option<HostEnvironment>,
        timeout: Duration,
    ) -> Self {
        let mut inner = Inner {
            enabled,
            command,
            environment,
            timeout,
            state: Mutex::new(HostRestartStatus {
                machine: machine.to_owned(),
                available: false,
                phase: HostRestartPhase::Unavailable,
                requested_at_ms: None,
                finished_at_ms: None,
                message: String::new(),
            }),
        };
        let (available, message) = match inner.probe() {
            Ok(_) if inner.environment.is_none() => (false, NO_IDENTITY_MESSAGE.to_owned()),
            Ok(_) => (true, READY_MESSAGE.to_owned()),
            Err(message) => (false, message),
        };
        {
            let state = inner.state.get_mut();
            state.available = available;
            state.phase = if available {
                HostRestartPhase::Idle
            } else {
                HostRestartPhase::Unavailable
            };
            state.message = message;
        }
        Self {
            inner: Arc::new(inner),
        }
    }

    /// A runner for control-plane tests: the command does not exist, so every
    /// start fails closed without spawning anything.
    #[cfg(test)]
    pub(crate) fn detached(machine: &str) -> Self {
        Self::new(
            machine,
            true,
            vec!["/nonexistent/atmux-restart/reboot".to_owned()],
            None,
            RUN_TIMEOUT,
        )
    }

    #[cfg(test)]
    fn fixture(machine: &str, command: &[&str], home: &Path) -> Self {
        Self::new(
            machine,
            true,
            command.iter().map(|part| (*part).to_owned()).collect(),
            Some(HostEnvironment::fixture(home)),
            RUN_TIMEOUT,
        )
    }

    pub async fn status(&self) -> HostRestartStatus {
        let mut state = self.inner.state.lock().await;
        if state.phase != HostRestartPhase::Restarting {
            match self.inner.probe() {
                Ok(_) if self.inner.environment.is_none() => {
                    state.available = false;
                    state.phase = HostRestartPhase::Unavailable;
                    NO_IDENTITY_MESSAGE.clone_into(&mut state.message);
                }
                Ok(_) => {
                    state.available = true;
                    if state.phase == HostRestartPhase::Unavailable {
                        state.phase = HostRestartPhase::Idle;
                        READY_MESSAGE.clone_into(&mut state.message);
                    }
                }
                Err(message) => {
                    state.available = false;
                    state.phase = HostRestartPhase::Unavailable;
                    state.message = message;
                }
            }
        }
        state.clone()
    }

    /// Runs the fixed reboot command and returns at once with `restarting`.
    ///
    /// Output is discarded, so a misconfigured command can never leak through
    /// the API.  A failed or timed-out attempt leaves the runner ready for
    /// another request, because a reboot that did not happen is worth retrying
    /// once the operator has fixed the cause.
    ///
    /// # Errors
    ///
    /// Returns [`HostRestartStartError::Running`] while a command is in
    /// flight, or [`HostRestartStartError::Unavailable`] when no command can
    /// run on this machine.
    pub async fn start(&self) -> Result<HostRestartStatus, HostRestartStartError> {
        let mut state = self.inner.state.lock().await;
        if state.phase == HostRestartPhase::Restarting {
            return Err(HostRestartStartError::Running);
        }
        let command = match self.inner.probe() {
            Ok(command) => command,
            Err(message) => {
                state.available = false;
                state.phase = HostRestartPhase::Unavailable;
                state.message = message;
                return Err(HostRestartStartError::Unavailable(state.message.clone()));
            }
        };
        let Some(environment) = self.inner.environment.clone() else {
            state.available = false;
            state.phase = HostRestartPhase::Unavailable;
            NO_IDENTITY_MESSAGE.clone_into(&mut state.message);
            return Err(HostRestartStartError::Unavailable(state.message.clone()));
        };
        state.available = true;
        state.phase = HostRestartPhase::Restarting;
        state.requested_at_ms = Some(now_ms());
        state.finished_at_ms = None;
        RUNNING_MESSAGE.clone_into(&mut state.message);
        let started = state.clone();
        drop(state);

        let runner = self.clone();
        tokio::spawn(async move {
            let outcome = run_command(&command, &environment, runner.inner.timeout).await;
            let mut state = runner.inner.state.lock().await;
            state.finished_at_ms = Some(now_ms());
            match outcome {
                CommandOutcome::Succeeded => {
                    state.phase = HostRestartPhase::Requested;
                    REQUESTED_MESSAGE.clone_into(&mut state.message);
                }
                CommandOutcome::Failed(code) => {
                    state.phase = HostRestartPhase::Failed;
                    state.message = failure_message(&command, code);
                }
                CommandOutcome::TimedOut => {
                    state.phase = HostRestartPhase::Failed;
                    state.message = format!(
                        "Restart command did not finish within {} seconds",
                        runner.inner.timeout.as_secs()
                    );
                }
            }
        });
        Ok(started)
    }
}

/// A failure message with the one hint operators need most: `sudo -n` exits 1
/// when it would have asked for a password.
fn failure_message(command: &[String], code: Option<i32>) -> String {
    let uses_sudo = command.first().is_some_and(|program| {
        Path::new(program)
            .file_name()
            .is_some_and(|name| name == "sudo")
    });
    match code {
        Some(1) if uses_sudo => {
            "Restart command exited with status 1; sudo needs a passwordless entry for it"
                .to_owned()
        }
        Some(code) => format!("Restart command exited with status {code}"),
        None => "Restart command was terminated".to_owned(),
    }
}

async fn run_command(
    command: &[String],
    environment: &HostEnvironment,
    timeout: Duration,
) -> CommandOutcome {
    let Some((program, arguments)) = command.split_first() else {
        return CommandOutcome::Failed(None);
    };
    let mut child_command = Command::new(program);
    configure_script_environment(&mut child_command, environment);
    child_command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let Ok(mut child) = child_command.spawn() else {
        return CommandOutcome::Failed(None);
    };
    let Some(group) = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
    else {
        let _ = child.kill().await;
        return CommandOutcome::Failed(None);
    };
    let deadline = Instant::now() + timeout;
    match tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        child.wait(),
    )
    .await
    {
        Ok(Ok(status)) if status.success() => CommandOutcome::Succeeded,
        Ok(Ok(status)) => CommandOutcome::Failed(status.code()),
        Ok(Err(_)) => CommandOutcome::Failed(None),
        Err(_) => {
            terminate_process_group(&mut child, group).await;
            CommandOutcome::TimedOut
        }
    }
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
    use super::*;

    async fn settled(runner: &HostRestartRunner) -> HostRestartStatus {
        for _ in 0..100 {
            let status = runner.status().await;
            if status.phase != HostRestartPhase::Restarting {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("restart command never settled");
    }

    #[test]
    fn default_command_is_absolute_and_platform_specific() {
        let command = default_command();
        assert!(Path::new(&command[0]).is_absolute());
        if cfg!(target_os = "macos") {
            assert_eq!(command[0], "/usr/bin/osascript");
        } else {
            assert_eq!(command[0], "/usr/bin/sudo");
            assert_eq!(command[1], "-n");
            assert_eq!(command.last().map(String::as_str), Some("reboot"));
        }
    }

    #[tokio::test]
    async fn missing_or_disabled_commands_fail_closed_before_spawning() {
        let home = std::env::temp_dir();
        let missing = HostRestartRunner::fixture("box", &["/nonexistent/reboot"], &home);
        let status = missing.status().await;
        assert!(!status.available);
        assert_eq!(status.phase, HostRestartPhase::Unavailable);
        assert!(status.message.contains("/nonexistent/reboot"));
        assert!(matches!(
            missing.start().await,
            Err(HostRestartStartError::Unavailable(_))
        ));

        let disabled = HostRestartRunner::new(
            "box",
            false,
            vec!["/bin/sh".to_owned()],
            Some(HostEnvironment::fixture(&home)),
            RUN_TIMEOUT,
        );
        assert_eq!(disabled.status().await.message, DISABLED_MESSAGE);
        assert_eq!(
            disabled.start().await,
            Err(HostRestartStartError::Unavailable(
                DISABLED_MESSAGE.to_owned()
            ))
        );

        // A command the node cannot run with a known identity is not offered.
        let detached = HostRestartRunner::detached("box");
        assert!(!detached.status().await.available);
    }

    #[tokio::test]
    async fn a_successful_command_reports_the_restart_as_requested() {
        let home = std::env::temp_dir();
        let runner = HostRestartRunner::fixture("box", &["/bin/sh", "-c", "exit 0"], &home);
        let ready = runner.status().await;
        assert!(ready.available);
        assert_eq!(ready.phase, HostRestartPhase::Idle);
        assert_eq!(ready.message, READY_MESSAGE);

        let started = runner.start().await.unwrap();
        assert_eq!(started.phase, HostRestartPhase::Restarting);
        assert!(started.requested_at_ms.is_some());
        let done = settled(&runner).await;
        assert_eq!(done.phase, HostRestartPhase::Requested);
        assert_eq!(done.message, REQUESTED_MESSAGE);
        assert!(done.finished_at_ms.is_some());
        assert!(done.available);
    }

    #[tokio::test]
    async fn failures_and_timeouts_leave_the_runner_ready_for_another_attempt() {
        let home = std::env::temp_dir();
        let failing = HostRestartRunner::fixture("box", &["/bin/sh", "-c", "exit 3"], &home);
        failing.start().await.unwrap();
        let failed = settled(&failing).await;
        assert_eq!(failed.phase, HostRestartPhase::Failed);
        assert_eq!(failed.message, "Restart command exited with status 3");
        // Another attempt is accepted once the first has finished.
        assert_eq!(
            failing.start().await.unwrap().phase,
            HostRestartPhase::Restarting
        );

        let slow = HostRestartRunner::fixture("box", &["/bin/sleep", "30"], &home);
        slow.start().await.unwrap();
        assert_eq!(slow.start().await, Err(HostRestartStartError::Running));
        let timed_out = settled(&slow).await;
        assert_eq!(timed_out.phase, HostRestartPhase::Failed);
        assert!(timed_out.message.contains("did not finish"));
    }

    #[test]
    fn sudo_password_failures_get_a_hint() {
        let sudo = vec!["/usr/bin/sudo".to_owned(), "-n".to_owned()];
        assert!(failure_message(&sudo, Some(1)).contains("passwordless"));
        assert_eq!(
            failure_message(&sudo, Some(2)),
            "Restart command exited with status 2"
        );
        let plain = vec!["/sbin/reboot".to_owned()];
        assert_eq!(
            failure_message(&plain, Some(1)),
            "Restart command exited with status 1"
        );
        assert_eq!(
            failure_message(&plain, None),
            "Restart command was terminated"
        );
    }

    #[test]
    fn status_serializes_without_the_command_line() {
        let status = HostRestartStatus {
            machine: "box".to_owned(),
            available: true,
            phase: HostRestartPhase::Requested,
            requested_at_ms: Some(1),
            finished_at_ms: None,
            message: REQUESTED_MESSAGE.to_owned(),
        };
        let encoded = serde_json::to_string(&status).unwrap();
        assert!(encoded.contains("\"phase\":\"requested\""));
        assert!(!encoded.contains("finished_at_ms"));
        assert!(!encoded.contains("sudo") && !encoded.contains("osascript"));
    }
}
