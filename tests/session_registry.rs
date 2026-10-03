//! End-to-end owner scans and dashboard closes, using only a disposable socket
//! and an isolated HOME. The fixture CLI is a copy of sh named `claude`.
use atmux::{
    config::MachineConfig,
    events::EventPage,
    machine::now_ms,
    registry::{RegistryPage, SessionState, SessionsPage},
    remote::RemoteMachine,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct DisposableOwner {
    directory: PathBuf,
    socket: String,
    web: Option<Child>,
}
impl Drop for DisposableOwner {
    fn drop(&mut self) {
        if let Some(child) = &mut self.web {
            let _ = child.kill();
            let _ = child.wait();
        }
        for name in ["external", "dashboard"] {
            let _ = Command::new("tmux")
                .args(["-L", &self.socket, "kill-session", "-t", name])
                .env_remove("TMUX")
                .env_remove("TMUX_PANE")
                .output();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}
impl DisposableOwner {
    fn tmux(&self, args: &[&str]) -> String {
        let output = Command::new("tmux")
            .args(["-L", &self.socket])
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn create_agent(&self, name: &str) {
        let cwd = self.directory.join("project");
        let claude = self.directory.join("home/bin/claude");
        let command = format!(
            "exec {} -c {}",
            shell_words::quote(claude.to_str().unwrap()),
            shell_words::quote("printf 'Claude Code fixture\n'; sleep 300; :")
        );
        self.tmux(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            name,
            "-c",
            cwd.to_str().unwrap(),
            &command,
        ]);
        let pid = self
            .tmux(&["display-message", "-p", "-t", name, "#{pane_pid}"])
            .parse::<u32>()
            .unwrap();
        let id = if name == "external" {
            "0199a5b7-5560-7abc-8def-0123456789ab"
        } else {
            "0199a5b7-5560-7abc-8def-0123456789ac"
        };
        let encoded = cwd
            .to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let root = self.directory.join("home/.claude");
        let log = root
            .join("projects")
            .join(encoded)
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(
            &log,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"archive fixture\"}}\n",
        )
        .unwrap();
        fs::write(root.join(format!("sessions/{pid}.json")), serde_json::to_vec(&serde_json::json!({"pid": pid, "cwd": cwd, "startedAt": now_ms(), "sessionId": id})).unwrap()).unwrap();
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Complete disposable owner lifecycle is kept together for cleanup.
async fn disposable_owner_archives_external_and_dashboard_kills() {
    let usable = Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|v| v.status.success());
    if !usable {
        assert!(
            std::env::var_os("ATMUX_REQUIRE_TMUX").is_none(),
            "tmux required"
        );
        return;
    }
    let socket = format!("atmux-test-a3-{}-{}", std::process::id(), now_ms());
    let directory = std::env::temp_dir().join(&socket);
    fs::create_dir(&directory).unwrap();
    let directory = directory.canonicalize().unwrap();
    let mut owner = DisposableOwner {
        directory,
        socket,
        web: None,
    };
    fs::create_dir_all(owner.directory.join("home/bin")).unwrap();
    fs::create_dir(owner.directory.join("project")).unwrap();
    let runtime = owner.directory.join("runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    fs::copy("/bin/sh", owner.directory.join("home/bin/claude")).unwrap();
    owner.create_agent("external");
    owner.create_agent("dashboard");
    let token = owner.directory.join("node.token");
    fs::write(&token, "a3-disposable-owner-token").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let config_path = owner.directory.join("config.toml");
    let config = format!(
        r#"profiles = []
[general]
project_roots = []
favorite_dirs = []
switch_on_launch = false
refresh_ms = 100
[node]
id = "fixture"
token_file = "{}"
[registry]
enabled = true
directory = "{}"
[events]
inject_hooks = false
directory = "{}"
[web]
allow_unauthenticated_loopback = true
"#,
        token.display(),
        owner.directory.join("registry").display(),
        owner.directory.join("events").display()
    );
    fs::write(&config_path, config).unwrap();
    let log = fs::File::create(owner.directory.join("web.log")).unwrap();
    owner.web = Some(
        Command::new(env!("CARGO_BIN_EXE_atmux"))
            .args([
                "--config",
                config_path.to_str().unwrap(),
                "web",
                "--bind",
                &address.to_string(),
            ])
            .env("ATMUX_TMUX_SOCKET_NAME", &owner.socket)
            .env("HOME", owner.directory.join("home"))
            .env("XDG_STATE_HOME", owner.directory.join("state"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("TMPDIR", &runtime)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let remote = RemoteMachine::from_config(&MachineConfig {
        id: "fixture".to_owned(),
        label: None,
        url: format!("http://{address}"),
        token_env: None,
        token_file: Some(token),
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let page = loop {
        let diagnostic = match remote
            .get_json::<RegistryPage>("/api/v1/registry?wait_ms=0")
            .await
        {
            Ok(page) => format!("{page:?}"),
            Err(error) => format!("{error:#}"),
        };
        if let Ok(page) = remote
            .get_json::<RegistryPage>("/api/v1/registry?wait_ms=0")
            .await
            && page.records.len() == 2
            && page.records.iter().all(|r| r.native.is_some())
        {
            break page;
        }
        assert!(
            Instant::now() < deadline,
            "owner did not map fixture native logs: {diagnostic}; child={:?}; log={}; overview={:?}; tmux={}",
            owner.web.as_mut().unwrap().try_wait().unwrap(),
            fs::read_to_string(owner.directory.join("web.log")).unwrap(),
            remote
                .get_json::<serde_json::Value>("/api/v1/sessions")
                .await,
            owner.tmux(&[
                "list-panes",
                "-a",
                "-F",
                "#{pane_current_command} #{pane_start_command} #{@atmux_session_key}"
            ])
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    owner.tmux(&["kill-session", "-t", "external"]);
    let dashboard = page
        .records
        .iter()
        .find(|r| r.record.name == "dashboard")
        .unwrap();
    remote
        .delete(&format!(
            "/api/v1/sessions/{}",
            atmux::remote::encode_segment(&dashboard.pane_id)
        ))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let archived = loop {
        let records = remote
            .get_json::<SessionsPage>("/api/v1/session-history?state=archived")
            .await
            .unwrap();
        if records.sessions.len() == 2 {
            break records;
        }
        assert!(Instant::now() < deadline, "sessions were not archived");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    for record in archived.sessions {
        assert_eq!(record.state, SessionState::Archived);
        let bundle = record.bundle.unwrap();
        assert!(bundle.native_log);
        assert!(
            page.records
                .iter()
                .any(|old| old.record.session_key == record.session_key)
        );
        let public = remote
            .get_json::<serde_json::Value>(&format!(
                "/api/v1/session-history/{}",
                record.session_key
            ))
            .await
            .unwrap();
        assert!(public.get("native").is_none());
        let events = remote.get_json::<EventPage>(&format!(
            "/api/v1/agent-events?session_key={}&types=agent.exited,session.closed,session.archived",
            record.session_key
        )).await.unwrap();
        assert_eq!(
            events
                .events
                .iter()
                .map(|stored| stored.event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["agent.exited", "session.closed", "session.archived"],
            "{} lifecycle events: {:#?}",
            record.name,
            events.events
        );
        let archived_event = &events.events[2].event;
        assert_eq!(archived_event.session_key, record.session_key);
        assert_eq!(archived_event.detail["state"], "archived");
        assert_eq!(archived_event.detail["archive_bundle_id"], bundle.id);
    }
}
