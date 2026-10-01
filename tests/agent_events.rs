//! Real Unix hook transport and authenticated federation; only disposable tmux.
use atmux::{
    config::Config,
    control::ControlPlane,
    events::{AgentEvent, EventPage, EventQuery, StoredEvent},
    remote::RemoteMachine,
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt as _;

struct Sandbox {
    path: PathBuf,
    socket: String,
    owner: Option<Child>,
}
impl Sandbox {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket = format!("atmux-test-events-{}-{nonce}", std::process::id());
        let path = std::env::temp_dir().join(&socket);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            path,
            socket,
            owner: None,
        }
    }
    fn tmux(&self, args: &[&str]) -> String {
        let output = Command::new("tmux")
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("XDG_RUNTIME_DIR", &self.path)
            .env("TMPDIR", &self.path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(child) = &mut self.owner {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output();
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn available_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn http_machine(port: u16) -> RemoteMachine {
    RemoteMachine::from_config(&atmux::config::MachineConfig {
        id: "fixture".into(),
        label: None,
        url: format!("http://127.0.0.1:{port}"),
        token_env: None,
        token_file: None,
    })
    .unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One isolated end-to-end transport scenario.
async fn disposable_pane_hook_delivers_needs_input_with_its_stable_session_key() {
    assert!(
        Command::new("tmux")
            .arg("-V")
            .output()
            .unwrap()
            .status
            .success(),
        "tmux is required for the agent-events acceptance test"
    );
    let mut sandbox = Sandbox::new();
    let port = available_port();
    let binary = env!("CARGO_BIN_EXE_atmux");
    let config = sandbox.path.join("config.toml");
    fs::write(&config, format!("[general]\nproject_roots=[]\nfavorite_dirs=[]\nrefresh_ms=100\n[web]\nallow_unauthenticated_loopback=true\n[node]\nid='fixture'\n[events]\ndirectory={}\n", toml::Value::String(sandbox.path.join("events").to_string_lossy().into_owned()))).unwrap();
    let script = sandbox.path.join("fake-agent.sh");
    let payload = include_str!("fixtures/agent-events/claude-notification.json");
    fs::write(&script, format!("#!/bin/sh\nwhile [ ! -S {} ]; do sleep 0.02; done\nprintf '%s' {} | {} hook claude\nsleep 30\n", shell_words::quote(sandbox.path.join("atmux/hooks.sock").to_str().unwrap()), shell_words::quote(payload), shell_words::quote(binary))).unwrap();
    let pane = sandbox.tmux(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-s",
        "fake-agent",
        &shell_words::join(["/bin/sh", script.to_str().unwrap()]),
    ]);
    sandbox.owner = Some(
        Command::new(binary)
            .args([
                "--config",
                config.to_str().unwrap(),
                "web",
                "--bind",
                &format!("127.0.0.1:{port}"),
            ])
            .env("ATMUX_TMUX_SOCKET_NAME", &sandbox.socket)
            .env("XDG_RUNTIME_DIR", &sandbox.path)
            .env("TMPDIR", &sandbox.path)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let remote = http_machine(port);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let found = loop {
        if let Ok(page) = remote
            .get_json::<EventPage>("/api/v1/agent-events?limit=100")
            .await
            && let Some(found) = page.events.into_iter().find(|v| {
                v.event.event_type == "agent.needs_input"
                    && v.event.reason.as_deref() == Some("permission")
            })
        {
            break found;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "hook event did not arrive; owner status {:?}",
            sandbox.owner.as_mut().unwrap().try_wait()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let session_key = sandbox.tmux(&[
        "display-message",
        "-p",
        "-t",
        &pane,
        "#{@atmux_session_key}",
    ]);
    assert_eq!(found.event.session_key, session_key);
    assert_eq!(found.event.pane, format!("fixture~{pane}"));
    assert_eq!(found.event.harness, "claude");
    assert_eq!(
        fs::metadata(sandbox.path.join("atmux"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(sandbox.path.join("atmux/hooks.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let snapshot: EventPage = remote
        .get_json("/api/v1/agent-events?limit=100")
        .await
        .unwrap();
    let cursor = snapshot.next;
    // Same-user spoof from outside the pane's process ancestry must be rejected.
    let mut spoof = Command::new(binary)
        .args(["hook", "claude"])
        .env("TMUX_PANE", &pane)
        .env("XDG_RUNTIME_DIR", &sandbox.path)
        .env("TMPDIR", &sandbox.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write as _;
        spoof
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
    }
    let output = spoof.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let after: EventPage = remote
        .get_json(&format!(
            "/api/v1/agent-events?after={}&wait=1",
            atmux::remote::encode_segment(&cursor)
        ))
        .await
        .unwrap();
    assert!(
        !after
            .events
            .iter()
            .any(|v| v.event.event_type == "agent.needs_input"
                && v.event.reason.as_deref() == Some("permission"))
    );
}

#[test]
fn hook_is_silent_and_returns_success_with_owner_down_or_unclosed_stdin() {
    let sandbox = Sandbox::new();
    let binary = env!("CARGO_BIN_EXE_atmux");
    let mut listener = None;
    for scenario in ["owner_down", "unclosed_stdin", "unresponsive_owner"] {
        if scenario == "unclosed_stdin" {
            let directory = sandbox.path.join("atmux");
            fs::create_dir_all(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let socket = directory.join("hooks.sock");
            listener = Some(std::os::unix::net::UnixListener::bind(&socket).unwrap());
            fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let started = std::time::Instant::now();
        let mut child = Command::new(binary)
            .args(["hook", "codex"])
            .env("XDG_RUNTIME_DIR", &sandbox.path)
            .env("TMPDIR", &sandbox.path)
            .env("TMUX_PANE", "%999")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = Some(child.stdin.take().unwrap());
        if scenario == "unresponsive_owner" {
            use std::io::Write as _;
            input
                .as_mut()
                .unwrap()
                .write_all(include_bytes!("fixtures/agent-events/codex-stop.json"))
                .unwrap();
            drop(input.take());
        }
        while child.try_wait().unwrap().is_none() {
            assert!(started.elapsed() < Duration::from_millis(500), "{scenario}");
            std::thread::sleep(Duration::from_millis(2));
        }
        drop(input);
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{scenario}");
        assert!(output.stdout.is_empty(), "{scenario}");
        assert!(output.stderr.is_empty(), "{scenario}");
    }
    drop(listener);
}

type ObservedRequest = (Option<String>, Option<String>);

#[derive(Clone)]
struct Fixture {
    event: AgentEvent,
    epoch: String,
    seen: Arc<Mutex<Vec<ObservedRequest>>>,
}
async fn feed(
    State(fixture): State<Fixture>,
    headers: HeaderMap,
    Query(query): Query<EventQuery>,
) -> Json<EventPage> {
    fixture.seen.lock().unwrap().push((
        headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_owned()),
        query.after.clone(),
    ));
    let next = format!("{}:1", fixture.epoch);
    Json(EventPage {
        epoch: fixture.epoch,
        events: if query.after.as_ref() == Some(&next) {
            vec![]
        } else {
            vec![StoredEvent {
                seq: 1,
                event: fixture.event,
            }]
        },
        next,
        reset: false,
    })
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Online and legacy owners share one fixture scenario.
async fn coordinator_federates_authenticated_owner_and_tolerates_old_404_owner() {
    let sandbox = Sandbox::new();
    let token = sandbox.path.join("token");
    fs::write(&token, "fixture-secret\n").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut event = AgentEvent::node_started("fixture").unwrap();
    event.event_type = "agent.needs_input".into();
    event.reason = Some("question".into());
    let epoch = event.id.clone();
    let fixture = Fixture {
        event: event.clone(),
        epoch,
        seen: seen.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = Router::new()
        .route("/api/v1/agent-events", get(feed))
        .route(
            "/old/api/v1/agent-events",
            get(|| async { StatusCode::NOT_FOUND }),
        )
        .with_state(fixture);
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut config: Config = toml::from_str(atmux::config::DEFAULT_CONFIG).unwrap();
    config.profiles.clear();
    config.general.project_roots.clear();
    config.general.favorite_dirs.clear();
    config.general.switch_on_launch = false;
    config.node.coordinator_only = true;
    config.node.id = "home".into();
    config.events = Some(atmux::events::EventsConfig {
        directory: Some(sandbox.path.join("home")),
        ..Default::default()
    });
    config.machines = vec![
        atmux::config::MachineConfig {
            id: "fixture".into(),
            label: None,
            url: format!("http://127.0.0.1:{port}"),
            token_env: None,
            token_file: Some(token),
        },
        atmux::config::MachineConfig {
            id: "old".into(),
            label: None,
            url: format!("http://127.0.0.1:{port}/old"),
            token_env: None,
            token_file: None,
        },
    ];
    // NodeUrl forbids prefixes: a separate 404 fixture represents an old owner.
    let old_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let old_port = old_listener.local_addr().unwrap().port();
    config.machines[1].url = format!("http://127.0.0.1:{old_port}");
    let old_server = tokio::spawn(async move {
        axum::serve(old_listener, Router::new()).await.unwrap();
    });
    let control = ControlPlane::start(config).await.unwrap();
    let page = control
        .agent_events(
            EventQuery {
                wait: Some(5),
                types: Some("agent.needs_input".into()),
                reasons: Some("question".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].event.id, event.id);
    let owner = control
        .agent_events(EventQuery::default(), true)
        .await
        .unwrap();
    assert!(
        owner.events.is_empty(),
        "fleet events must not re-export as owner events"
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let replay = control
        .agent_events(
            EventQuery {
                after: Some(page.next),
                ..Default::default()
            },
            false,
        )
        .await
        .unwrap();
    assert!(replay.events.is_empty());
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .all(|(header, _)| header.as_deref() == Some("Bearer fixture-secret"))
    );
    assert!(seen.iter().any(|(_, cursor)| cursor.is_some()));
    server.abort();
    old_server.abort();
}

#[tokio::test]
async fn disabled_event_api_is_404_and_query_bounds_are_400() {
    let mut config: Config = toml::from_str(atmux::config::DEFAULT_CONFIG).unwrap();
    config.profiles.clear();
    config.general.project_roots.clear();
    config.general.favorite_dirs.clear();
    config.general.switch_on_launch = false;
    config.node.coordinator_only = true;
    let control = ControlPlane::start(config).await.unwrap();
    let (_shutdown, receiver) = tokio::sync::watch::channel(false);
    let router = atmux::web::api_router(control, vec![], receiver);
    for (path, expected) in [
        ("/api/v1/agent-events", 404),
        ("/api/v1/fleet/agent-events?limit=0", 400),
    ] {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(value.get("error").is_some());
    }
}
