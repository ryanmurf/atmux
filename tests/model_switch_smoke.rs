use std::{
    env, fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use atmux::{config::ProfileMode, status::AgentKind, tmux::Tmux};

const FAKE_HARNESS: &str = r#"
import os, sys, termios, tty

harness = sys.argv[1]
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
tty.setraw(fd)

def out(value):
    os.write(sys.stdout.fileno(), value.encode())

if harness == "claude":
    models = ["Default", "Opus", "Fable", "Sonnet", "Haiku"]
    selected = 3
    out("Claude Code v2.1.226\r\nSonnet 5 with xhigh effort\r\n")
else:
    models = ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex-spark"]
    selected = 1
    out("OpenAI Codex (v0.147.0)\r\nmodel: gpt-5.6-terra xhigh /model to change\r\n")

state = "input"
line = b""
escape = b""
reason_selected = 1
try:
    while True:
        value = os.read(fd, 1)
        if not value:
            break
        if state == "input":
            if value in (b"\r", b"\n"):
                if line.endswith(b"/model"):
                    if harness == "claude":
                        out("Select model\r\n")
                    else:
                        out("Select Model and Effort\r\n")
                    for index, model in enumerate(models):
                        marker = "❯" if harness == "claude" and index == selected else ("›" if harness == "codex" and index == selected else " ")
                        out(f"{marker} {index + 1}. {model}\r\n")
                    state = "model"
                line = b""
            elif value >= b" ":
                line += value
        elif value == b"\x1b" or escape:
            escape += value
            if escape.endswith(b"[A"):
                if state == "model": selected = max(0, selected - 1)
                else: reason_selected = max(0, reason_selected - 1)
                escape = b""
            elif escape.endswith(b"[B"):
                if state == "model": selected = min(len(models) - 1, selected + 1)
                else: reason_selected = min(4, reason_selected + 1)
                escape = b""
            elif len(escape) > 3:
                escape = b""
        elif harness == "claude" and state == "model" and value == b"s":
            out(f"Set model to {models[selected]} for this session only\r\n")
            state = "input"
        elif harness == "codex" and state == "model" and value in (b"\r", b"\n"):
            out(f"Select Reasoning Level for {models[selected]}\r\n")
            labels = ["Low", "Medium (default)", "High", "Extra high", "More reasoning…"]
            for index, label in enumerate(labels):
                marker = "›" if index == reason_selected else " "
                out(f"{marker} {index + 1}. {label}\r\n")
            state = "reason"
        elif harness == "codex" and state == "reason" and value in (b"\r", b"\n"):
            out(f"Model changed to {models[selected]} xhigh\r\n")
            state = "input"
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
"#;

struct Probe {
    socket: String,
    directory: PathBuf,
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .status();
        remove_disposable_socket_file(&self.socket);
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn remove_disposable_socket_file(socket: &str) {
    if socket.is_empty()
        || socket.len() > 100
        || !socket
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return;
    }
    let Ok(executable) = env::current_exe() else {
        return;
    };
    let Ok(uid) = fs::metadata(executable).map(|metadata| metadata.uid()) else {
        return;
    };
    let root = env::var_os("TMUX_TMPDIR").map_or_else(env::temp_dir, PathBuf::from);
    let path = root.join(format!("tmux-{uid}")).join(socket);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
        let _ = fs::remove_file(path);
    }
}

fn start_probe() -> Probe {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = format!("atmux-model-{}-{nonce}", std::process::id());
    let directory = env::temp_dir().join(&socket);
    fs::create_dir(&directory).unwrap();
    let script = directory.join("fake_agent.py");
    fs::write(&script, FAKE_HARNESS).unwrap();
    for harness in ["claude", "codex"] {
        let command = shell_words::join(["python3", "-u", script.to_str().unwrap(), harness]);
        let status = Command::new("tmux")
            .args(["-L", &socket, "new-session", "-d", "-s", harness, &command])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .status()
            .unwrap();
        assert!(status.success());
    }
    thread::sleep(Duration::from_millis(200));
    Probe { socket, directory }
}

#[test]
fn claude_and_codex_switch_through_fixed_real_tmux_picker_flows() {
    let probe = start_probe();
    Tmux::with_socket_for_test(&probe.socket, || {
        Tmux.switch_model(
            "claude:0.0",
            AgentKind::Claude,
            "2.1.226",
            &ProfileMode {
                id: "fable".to_owned(),
                label: None,
                model: "fable".to_owned(),
                effort: None,
                service_tier: None,
            },
        )?;
        let claude = Tmux.capture("claude:0.0", 80)?;
        assert!(claude.contains("Set model to Fable for this session only"));
        assert_eq!(
            Tmux.model_observation("claude:0.0", AgentKind::Claude, &claude)
                .current
                .as_deref(),
            Some("fable")
        );

        Tmux.switch_model(
            "codex:0.0",
            AgentKind::Codex,
            "0.147.0",
            &ProfileMode {
                id: "sol-xhigh".to_owned(),
                label: None,
                model: "gpt-5.6-sol".to_owned(),
                effort: Some("xhigh".to_owned()),
                service_tier: None,
            },
        )?;
        let codex = Tmux.capture("codex:0.0", 80)?;
        assert!(codex.contains("Model changed to gpt-5.6-sol xhigh"));
        assert_eq!(
            Tmux.model_observation("codex:0.0", AgentKind::Codex, &codex)
                .current
                .as_deref(),
            Some("gpt-5.6-sol")
        );
        Ok(())
    })
    .unwrap();
}
