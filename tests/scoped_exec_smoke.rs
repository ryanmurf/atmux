#![cfg(target_os = "linux")]

use std::{
    env, fs,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _},
    path::PathBuf,
    process::{Command, Output},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use atmux::config::DEFAULT_CONFIG;

struct Cleanup {
    socket: String,
    directory: PathBuf,
    unit: Option<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output();
        if let Some(unit) = self.unit.as_deref() {
            let _ = user_systemctl(&["--user", "stop", unit]);
        }
        remove_disposable_socket_file(&self.socket);
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn user_systemctl(arguments: &[&str]) -> Output {
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

fn tmux(socket: &str, arguments: &[&str]) -> Output {
    Command::new("tmux")
        .arg("-L")
        .arg(socket)
        .args(arguments)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .output()
        .unwrap()
}

fn remove_disposable_socket_file(socket: &str) {
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

/// End-to-end proof for the hidden recovery bridge. This test uses only a
/// disposable named tmux socket; it never addresses the user's default server.
#[test]
#[ignore = "requires tmux and an active systemd user manager"]
#[allow(clippy::too_many_lines)] // One linear black-box recovery assertion sequence.
fn scoped_exec_loads_config_publishes_metadata_and_execs_in_place() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = format!("atmux-scoped-exec-{}-{nonce}", std::process::id());
    let directory = env::temp_dir().join(&socket);
    fs::create_dir(&directory).unwrap();
    let mut cleanup = Cleanup {
        socket,
        directory,
        unit: None,
    };
    let config_path = cleanup.directory.join("config.toml");
    let config = DEFAULT_CONFIG.replace(
        "# memory_max_bytes = 34359738368 # 32 GiB",
        "memory_max_bytes = 12884901888\nmemory_override_max_bytes = 51539607552",
    );
    fs::write(&config_path, config).unwrap();

    let uid = rustix::process::geteuid().as_raw();
    let script = concat!(
        "if test -t 0 && test -t 1; then echo pty=yes; else echo pty=no; fi; ",
        "printf 'xdg=%s\\n' \"$XDG_RUNTIME_DIR\"; ",
        "printf 'dbus=%s\\n' \"$DBUS_SESSION_BUS_ADDRESS\"; ",
        "IFS= read -r input; printf 'received=%s\\n' \"$input\"; sleep 30"
    );
    let command = shell_words::join([
        env!("CARGO_BIN_EXE_atmux"),
        "--config",
        config_path.to_str().unwrap(),
        "scoped-exec",
        "--recovery-service-memory-max-bytes",
        "60129542144",
        "--",
        "/bin/sh",
        "-lc",
        script,
    ]);
    let launched = tmux(
        &cleanup.socket,
        &[
            "new-session",
            "-d",
            "-s",
            "scoped-exec",
            "-c",
            cleanup.directory.to_str().unwrap(),
            &command,
        ],
    );
    assert!(launched.status.success(), "{:?}", launched.stderr);

    let deadline = Instant::now() + Duration::from_secs(5);
    let capture = loop {
        let captured = tmux(
            &cleanup.socket,
            &["capture-pane", "-p", "-t", "scoped-exec:0.0"],
        );
        let capture = String::from_utf8_lossy(&captured.stdout).into_owned();
        if capture.contains("pty=") {
            break capture;
        }
        assert!(Instant::now() < deadline, "scoped-exec output: {capture}");
        thread::sleep(Duration::from_millis(25));
    };
    assert!(capture.contains("pty=yes"), "{capture}");
    assert!(
        capture.contains(&format!("xdg=/run/user/{uid}")),
        "{capture}"
    );
    assert!(
        capture.contains(&format!("dbus=unix:path=/run/user/{uid}/bus")),
        "{capture}"
    );

    let unit = tmux(
        &cleanup.socket,
        &[
            "show-options",
            "-p",
            "-q",
            "-v",
            "-t",
            "scoped-exec:0.0",
            "@atmux_systemd_scope",
        ],
    );
    assert!(unit.status.success(), "{:?}", unit.stderr);
    let unit = String::from_utf8(unit.stdout).unwrap().trim().to_owned();
    assert!(unit.starts_with("atmux-tmux-spawn-") && unit.strip_suffix(".scope").is_some());
    cleanup.unit = Some(unit.clone());
    let memory = tmux(
        &cleanup.socket,
        &[
            "show-options",
            "-p",
            "-q",
            "-v",
            "-t",
            "scoped-exec:0.0",
            "@atmux_memory_max_bytes",
        ],
    );
    assert_eq!(
        String::from_utf8(memory.stdout).unwrap().trim(),
        "60129542144"
    );
    let systemd_memory =
        user_systemctl(&["--user", "show", &unit, "--property=MemoryMax", "--value"]);
    assert_eq!(
        String::from_utf8(systemd_memory.stdout).unwrap().trim(),
        "60129542144"
    );

    assert!(
        tmux(
            &cleanup.socket,
            &[
                "send-keys",
                "-l",
                "-t",
                "scoped-exec:0.0",
                "recovery round trip"
            ]
        )
        .status
        .success()
    );
    assert!(
        tmux(
            &cleanup.socket,
            &["send-keys", "-t", "scoped-exec:0.0", "Enter"]
        )
        .status
        .success()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let capture = tmux(
            &cleanup.socket,
            &["capture-pane", "-p", "-t", "scoped-exec:0.0"],
        );
        let capture = String::from_utf8_lossy(&capture.stdout);
        if capture.contains("received=recovery round trip") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "scoped-exec PTY output: {capture}"
        );
        thread::sleep(Duration::from_millis(25));
    }

    assert!(
        tmux(&cleanup.socket, &["kill-session", "-t", "scoped-exec"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let loaded = user_systemctl(&["--user", "show", &unit, "--property=LoadState", "--value"]);
        assert!(loaded.status.success(), "{:?}", loaded.stderr);
        let loaded = String::from_utf8_lossy(&loaded.stdout);
        if loaded.trim().is_empty() || loaded.trim() == "not-found" {
            break;
        }
        assert!(Instant::now() < deadline, "scope remained loaded: {loaded}");
        thread::sleep(Duration::from_millis(25));
    }
    cleanup.unit = None;
}
