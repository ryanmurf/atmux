use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use atmux::{
    attachment::{EncodedImage, ImageMessageRequest, deliver},
    config::{AgentProfile, StatusConfig},
    tmux::Tmux,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nattachment smoke fixture";

struct SocketCleanup {
    socket: String,
    directory: PathBuf,
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

fn isolated_socket() -> SocketCleanup {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = format!("atmux-image-{}-{nonce}", std::process::id());
    let directory = env::temp_dir().join(&socket);
    fs::create_dir(&directory).unwrap();
    SocketCleanup { socket, directory }
}

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

fn exercise_model_shaped_pane(tmux: &Tmux, directory: &Path, harness: &str) {
    let name = format!("image-{harness}");
    let profile = AgentProfile {
        name: format!("{harness} image smoke"),
        harness: harness.to_owned(),
        command: "/bin/sh".to_owned(),
        args: vec![
            "-lc".to_owned(),
            concat!(
                "printf 'image-smoke-ready\\n'; ",
                "IFS= read -r instruction; IFS= read -r path_line; ",
                "IFS= read -r separator; IFS= read -r label; IFS= read -r prompt; ",
                "attachment_path=${path_line#- }; ",
                "if [ -r \"$attachment_path\" ]; then ",
                "bytes=$(wc -c < \"$attachment_path\" | tr -d ' '); ",
                "rm -f -- \"$attachment_path\"; ",
                "printf 'attachment-readable:%s:%s\\n' \"$bytes\" \"$prompt\"; ",
                "else printf 'attachment-unreadable\\n'; fi; sleep 10"
            )
            .to_owned(),
        ],
        env: BTreeMap::new(),
        inherit_discovered: false,
        modes: Vec::new(),
    };
    tmux.launch(&name, directory, &profile, None).unwrap();
    let sessions = tmux
        .sessions(&HashMap::default(), &StatusConfig::default())
        .unwrap();
    let pane_id = sessions
        .iter()
        .find(|session| session.name == name)
        .expect("image smoke session")
        .pane_id
        .clone();
    wait_for_capture(tmux, &pane_id, "image-smoke-ready");

    deliver(
        &pane_id,
        ImageMessageRequest {
            text: format!("inspect with {harness}"),
            images: vec![EncodedImage {
                media_type: "image/png".to_owned(),
                data: STANDARD.encode(PNG),
            }],
        },
        false,
    )
    .unwrap();
    let capture = wait_for_capture(tmux, &pane_id, "attachment-readable:");
    assert!(!capture.contains("attachment-unreadable"), "{capture}");
    let result = capture
        .lines()
        .find(|line| line.contains("attachment-readable:"))
        .expect("attachment result")
        .trim();
    let mut fields = result.splitn(3, ':');
    assert_eq!(fields.next(), Some("attachment-readable"));
    assert_eq!(
        fields.next().and_then(|bytes| bytes.parse::<usize>().ok()),
        Some(PNG.len())
    );
    assert_eq!(
        fields.next(),
        Some(format!("inspect with {harness}").as_str())
    );
}

#[test]
fn image_messages_reach_claude_and_codex_shaped_tmux_panes() {
    let Some(tmux) = tmux_or_skip("image_messages_reach_claude_and_codex_shaped_tmux_panes") else {
        return;
    };
    let probe = isolated_socket();
    Tmux::with_socket_for_test(&probe.socket, || {
        exercise_model_shaped_pane(&tmux, &probe.directory, "claude");
        exercise_model_shaped_pane(&tmux, &probe.directory, "codex");
        Ok(())
    })
    .unwrap();
}
