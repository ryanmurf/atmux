use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use atmux::{
    config::{AgentProfile, StatusConfig},
    tmux::Tmux,
};

struct SocketCleanup {
    socket: String,
    directory: PathBuf,
}

fn isolated_socket(label: &str) -> SocketCleanup {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = format!("atmux-{label}-{}-{nonce}", std::process::id());
    let directory = env::temp_dir().join(&socket);
    fs::create_dir(&directory).unwrap();
    SocketCleanup { socket, directory }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output();
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

/// Returns a tmux handle, or reports a skip.
///
/// CI sets `ATMUX_REQUIRE_TMUX` so a missing tmux fails loudly instead of
/// quietly deleting this suite from the run.
fn tmux_or_skip(test_name: &str) -> Option<Tmux> {
    match Tmux::check() {
        Ok(()) => Some(Tmux),
        Err(error) => {
            assert!(
                env::var_os("ATMUX_REQUIRE_TMUX").is_none(),
                "{test_name} requires tmux and ATMUX_REQUIRE_TMUX is set, but tmux is unusable: {error:#}"
            );
            eprintln!("SKIPPED {test_name}: tmux is unavailable: {error:#}");
            None
        }
    }
}

fn wait_for_capture(tmux: &Tmux, pane_id: &str, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let diagnostic = match tmux.capture(pane_id, 40) {
            Ok(capture) => {
                if capture.contains(expected) {
                    return capture;
                }
                format!("last capture: {capture:?}")
            }
            Err(error) => format!("last error: {error:#}"),
        };
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected:?}; {diagnostic}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn launches_discovers_captures_and_kills_a_session() {
    let Some(tmux) = tmux_or_skip("launches_discovers_captures_and_kills_a_session") else {
        return;
    };

    let probe = isolated_socket("launch-smoke");
    let name = "agent".to_owned();
    let profile = AgentProfile {
        name: "Smoke".to_owned(),
        harness: "test".to_owned(),
        command: "/bin/sh".to_owned(),
        args: vec![
            "-lc".to_owned(),
            "printf 'atmux-smoke-ready\\n'; IFS= read -r first; IFS= read -r second; printf 'received:%s|%s\\n' \"$first\" \"$second\"; sleep 10".to_owned(),
        ],
        env: BTreeMap::new(),
        inherit_discovered: false,
        modes: Vec::new(),
    };

    Tmux::with_socket_for_test(&probe.socket, || {
        tmux.launch(&name, &probe.directory, &profile, None)?;
        let sessions = tmux.sessions(&HashMap::default(), &StatusConfig::default())?;
        let session = sessions
            .iter()
            .find(|session| session.name == name)
            .expect("launched session should be discoverable");
        let identity = Command::new("tmux")
            .args([
                "-L",
                &probe.socket,
                "display-message",
                "-p",
                "-t",
                &session.pane_id,
                "#{@atmux_identity}",
            ])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()?;
        assert!(identity.status.success());
        let identity = String::from_utf8(identity.stdout)?.trim().to_owned();
        assert_eq!(identity.len(), "pane-v1-".len() + 64);
        assert!(identity.strip_prefix("pane-v1-").is_some_and(|digest| {
            digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        }));
        wait_for_capture(&tmux, &session.pane_id, "atmux-smoke-ready");
        tmux.send_text(
            &session.pane_id,
            "hello from atmux\nsecond literal line",
            true,
        )?;
        wait_for_capture(
            &tmux,
            &session.pane_id,
            "received:hello from atmux|second literal line",
        );
        tmux.kill(&name)
    })
    .unwrap();
}

#[test]
fn bracketed_paste_settles_before_submit_reaches_the_agent_tui() {
    let Some(_) = tmux_or_skip("bracketed_paste_settles_before_submit_reaches_the_agent_tui")
    else {
        return;
    };
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = format!("atmux-submit-{}-{nonce}", std::process::id());
    let directory = env::temp_dir().join(&socket);
    fs::create_dir(&directory).unwrap();
    let script = directory.join("paste_probe.py");
    fs::write(
        &script,
        r#"import os, sys, termios, time, tty
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
tty.setraw(fd)
os.write(sys.stdout.fileno(), b"\x1b[?2004hsubmit-probe-ready\r\n")
seen = b""
paste_ended = None
try:
    while True:
        value = os.read(fd, 1)
        if not value:
            break
        seen += value
        if seen.endswith(b"\x1b[201~"):
            paste_ended = time.monotonic()
        elif value in (b"\r", b"\n") and paste_ended is not None:
            elapsed = time.monotonic() - paste_ended
            outcome = b"submitted" if elapsed >= 0.05 and b"raw pane message" in seen else b"enter-too-early"
            os.write(sys.stdout.fileno(), outcome + b"\r\n")
            time.sleep(1.0)
            break
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
"#,
    )
    .unwrap();
    let command = shell_words::join(["python3", "-u", script.to_str().unwrap()]);
    let status = Command::new("tmux")
        .args(["-L", &socket, "new-session", "-d", "-s", "submit", &command])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .status()
        .unwrap();
    assert!(status.success());
    let _cleanup = SocketCleanup {
        socket: socket.clone(),
        directory,
    };
    thread::sleep(Duration::from_millis(150));

    Tmux::with_socket_for_test(&socket, || {
        Tmux.send_text("submit:0.0", "raw pane message", true)?;
        let capture = Tmux.capture("submit:0.0", 20)?;
        assert!(capture.contains("submitted"), "{capture:?}");
        assert!(!capture.contains("enter-too-early"), "{capture:?}");
        Ok(())
    })
    .unwrap();
}

#[test]
fn launch_reports_a_command_that_exits_immediately() {
    let Some(tmux) = tmux_or_skip("launch_reports_a_command_that_exits_immediately") else {
        return;
    };

    let probe = isolated_socket("exit-smoke");
    let name = "agent".to_owned();
    let profile = AgentProfile {
        name: "Immediate exit".to_owned(),
        harness: "test".to_owned(),
        command: "/bin/sh".to_owned(),
        args: vec!["-lc".to_owned(), "exit 7".to_owned()],
        env: BTreeMap::new(),
        inherit_discovered: false,
        modes: Vec::new(),
    };

    let error = Tmux::with_socket_for_test(&probe.socket, || {
        tmux.launch(&name, &probe.directory, &profile, None)
    })
    .expect_err("an immediately exiting command must not be reported as launched");
    assert!(
        error.to_string().contains("exited before it became ready"),
        "unexpected launch error: {error:#}"
    );
}
