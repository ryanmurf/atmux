use std::{
    collections::{BTreeMap, HashMap},
    env,
    process::Command,
    thread,
    time::Duration,
};

use atmux::{
    config::{AgentProfile, StatusConfig},
    tmux::Tmux,
};

struct SessionCleanup(String);

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.0])
            .output();
    }
}

#[test]
fn launches_discovers_captures_and_kills_a_session() {
    if Tmux::check().is_err() {
        eprintln!("tmux is unavailable; skipping integration smoke test");
        return;
    }

    let tmux = Tmux;
    let name = format!("atmux-smoke-{}", std::process::id());
    let cleanup = SessionCleanup(name.clone());
    let profile = AgentProfile {
        name: "Smoke".to_owned(),
        harness: "test".to_owned(),
        command: "/bin/sh".to_owned(),
        args: vec![
            "-lc".to_owned(),
            "printf 'atmux-smoke-ready\\n'; sleep 10".to_owned(),
        ],
        env: BTreeMap::new(),
    };

    tmux.launch(&name, &env::temp_dir(), &profile).unwrap();
    thread::sleep(Duration::from_millis(150));

    let sessions = tmux
        .sessions(&HashMap::default(), &StatusConfig::default())
        .unwrap();
    let session = sessions
        .iter()
        .find(|session| session.name == name)
        .expect("launched session should be discoverable");
    let captured = tmux.capture(&session.pane_id, 20).unwrap();
    assert!(captured.contains("atmux-smoke-ready"));

    tmux.kill(&name).unwrap();
    std::mem::forget(cleanup);
}
