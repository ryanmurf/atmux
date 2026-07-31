use std::{
    collections::HashMap,
    env,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};

use crate::{
    config::{AgentProfile, StatusConfig},
    status::{self, AgentKind, AgentStatus},
};

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
    pub path: PathBuf,
    pub command: String,
    pub title: String,
    pub content: String,
    pub content_hash: u64,
    pub agent: AgentKind,
    pub status: AgentStatus,
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
    command: String,
    path: PathBuf,
    title: String,
    pane_id: String,
    status_override: String,
}

impl RawPane {
    fn score(&self) -> u8 {
        u8::from(self.window_active) * 2 + u8::from(self.pane_active)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Tmux;

impl Tmux {
    /// Verifies that a working tmux executable is available.
    ///
    /// # Errors
    ///
    /// Returns an error when tmux is missing or exits unsuccessfully.
    pub fn check() -> Result<()> {
        let output = Command::new("tmux")
            .arg("-V")
            .output()
            .context("tmux is required but was not found in PATH")?;
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
            "#{pane_current_path}",
            "#{pane_title}",
            "#{pane_id}",
            "#{@atmux_status}",
        ]
        .join("\t");
        let output = Self::output(["list-panes", "-a", "-F", &format])?;
        let mut selected: HashMap<String, RawPane> = HashMap::new();
        for line in output.lines().filter(|line| !line.trim().is_empty()) {
            let Some(pane) = parse_pane(line) else {
                continue;
            };
            let should_replace = selected
                .get(&pane.name)
                .is_none_or(|existing| pane.score() > existing.score());
            if should_replace {
                selected.insert(pane.name.clone(), pane);
            }
        }

        let process_table = ProcessTable::load();
        let mut sessions = Vec::with_capacity(selected.len());
        for pane in selected.into_values() {
            let content = self.capture(&pane.pane_id, 36).unwrap_or_default();
            let content_hash = hash(&content);
            let changed = previous_hashes
                .get(&pane.pane_id)
                .is_some_and(|previous| *previous != content_hash);
            let process_tree = process_table.commands_under(pane.pane_pid);
            let agent = status::detect_kind(&pane.command, &process_tree);
            let agent_status = status::classify(
                agent,
                &content,
                &pane.title,
                &pane.status_override,
                changed,
                status_config,
            );
            sessions.push(Session {
                name: pane.name,
                attached: pane.attached,
                windows: pane.windows,
                activity: pane.activity,
                window_index: pane.window_index,
                pane_index: pane.pane_index,
                pane_id: pane.pane_id,
                pane_pid: pane.pane_pid,
                path: pane.path,
                command: pane.command,
                title: pane.title,
                content,
                content_hash,
                agent,
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

    /// Creates a detached session running the chosen agent profile.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid directory or a failed tmux launch.
    pub fn launch(&self, name: &str, directory: &Path, profile: &AgentProfile) -> Result<()> {
        if !command_available(&profile.command) {
            bail!(
                "agent command was not found or is not executable: {}",
                profile.command
            );
        }
        let mut invocation = vec!["env".to_owned()];
        invocation.extend(
            profile
                .env
                .iter()
                .map(|(key, value)| format!("{key}={value}")),
        );
        invocation.push(profile.command.clone());
        invocation.extend(profile.args.clone());
        let shell_command = shell_words::join(invocation);
        let directory = directory
            .to_str()
            .with_context(|| format!("directory is not valid UTF-8: {}", directory.display()))?;
        Self::output([
            "new-session",
            "-d",
            "-s",
            name,
            "-c",
            directory,
            &shell_command,
        ])?;
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
        let status = Command::new("tmux")
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
        Self::output(["kill-session", "-t", name]).map(|_| ())
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

    fn output<const N: usize>(args: [&str; N]) -> Result<String> {
        let summary = format!("tmux {}", args.join(" "));
        let output = Command::new("tmux")
            .args(args)
            .output()
            .with_context(|| format!("failed to run {summary}"))?;
        check_output(&output, &summary)
    }
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
    if fields.len() < 14 {
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
        path: PathBuf::from(fields[10]),
        title: fields[11].to_owned(),
        pane_id: fields[12].to_owned(),
        status_override: fields[13].to_owned(),
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
        "tmux",
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
    entries: HashMap<u32, (u32, String)>,
}

impl ProcessTable {
    fn load() -> Self {
        let Ok(output) = Command::new("ps")
            .args(["-axo", "pid=,ppid=,command="])
            .output()
        else {
            return Self::default();
        };
        let source = String::from_utf8_lossy(&output.stdout);
        let mut entries = HashMap::new();
        for line in source.lines() {
            let mut fields = line.split_whitespace();
            let (Some(pid), Some(parent)) = (fields.next(), fields.next()) else {
                continue;
            };
            let (Ok(pid), Ok(parent)) = (pid.parse(), parent.parse()) else {
                continue;
            };
            entries.insert(pid, (parent, fields.collect::<Vec<_>>().join(" ")));
        }
        Self { entries }
    }

    fn commands_under(&self, root: u32) -> String {
        self.entries
            .iter()
            .filter_map(|(&pid, (_, command))| self.descends_from(pid, root).then_some(command))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn descends_from(&self, mut pid: u32, root: u32) -> bool {
        for _ in 0..64 {
            if pid == root {
                return true;
            }
            let Some((parent, _)) = self.entries.get(&pid) else {
                return false;
            };
            if *parent == 0 || *parent == pid {
                return false;
            }
            pid = *parent;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tmux_pane() {
        let line = "work\t1\t2\t123\t0\t1\t1\t1\t42\tnode\t/tmp/work\t⠹ work\t%7\twaiting";
        let pane = parse_pane(line).unwrap();
        assert_eq!(pane.name, "work");
        assert_eq!(pane.pane_id, "%7");
        assert_eq!(pane.status_override, "waiting");
        assert_eq!(pane.score(), 3);
    }

    #[test]
    fn parses_pane_with_empty_status_override() {
        let line = "solo\t0\t1\t123\t0\t1\t0\t1\t42\tbash\t/tmp\tsolo\t%0\t";
        let pane = parse_pane(line).unwrap();
        assert_eq!(pane.name, "solo");
        assert!(pane.status_override.is_empty());
    }

    #[test]
    fn validates_agent_commands() {
        assert!(command_available("sh"));
        assert!(command_available("/bin/sh"));
        assert!(!command_available("/definitely/missing/atmux-agent"));
    }

    #[test]
    fn popup_command_reuses_the_current_tmux_socket() {
        let command = popup_attach_command("review one", "/tmp/tmux-1000/custom,42,0").unwrap();
        assert_eq!(
            shell_words::split(&command).unwrap(),
            [
                "env",
                "-u",
                "TMUX",
                "-u",
                "TMUX_PANE",
                "tmux",
                "-S",
                "/tmp/tmux-1000/custom",
                "attach-session",
                "-t",
                "review one",
            ]
        );
    }
}
