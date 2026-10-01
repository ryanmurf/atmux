//! The complete configured coordinator loop, with loopback OAuth, ledger,
//! GitHub and Qwen fixtures. Every tmux command names a disposable socket.
#![allow(clippy::too_many_lines)] // Full lifecycle fixtures keep teardown with setup.
use atmux::{
    config::{Config, MachineConfig},
    control::{ControlPlane, Overview},
    events::EventPage,
    github::GithubConfig,
    herodevs::{AuthConfig, HerodevsConfig},
    llm::LlmConfig,
    machine::now_ms,
    registry::{RegistryPage, SessionState},
    remote::RemoteMachine,
    supervisor::{Guard, GuardedMessage, ProjectStatus, SupervisorConfig},
};
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

struct DisposableOwner {
    directory: PathBuf,
    socket: String,
    web: Option<Child>,
}
impl DisposableOwner {
    fn new() -> Self {
        let socket = format!(
            "atmux-test-a6-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(&socket);
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            directory: directory.canonicalize().unwrap(),
            socket,
            web: None,
        }
    }
    fn tmux(&self, args: &[&str]) -> String {
        let output = Command::new("tmux")
            .args(["-L", &self.socket, "-f", "/dev/null"])
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
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
    fn create_agent(&self, name: &str) {
        let root = &self.directory;
        let command=match name {
            "permission"=>"printf 'Claude Code fixture\\nAllow command?\\nCommand: cargo test --all-features\\n'".to_owned(),
            "danger"=>"printf 'Claude Code fixture\\nAllow command?\\nCommand: git push --force\\n'".to_owned(),
            _=>"printf 'Claude Code fixture\\nJOB DONE job-done: TESTS PASSED https://github.com/org/repo/pull/1\\n'".to_owned(),
        };
        let payload = if name == "done" {
            r#"{"hook_event_name":"Stop"}"#
        } else {
            r#"{"hook_event_name":"Notification","notification_type":"permission_prompt"}"#
        };
        let hook = format!(
            "while [ ! -S {} ]; do sleep 0.02; done; printf '%s' {} | {} hook claude",
            shell_words::quote(root.join("runtime/atmux/hooks.sock").to_str().unwrap()),
            shell_words::quote(payload),
            shell_words::quote(env!("CARGO_BIN_EXE_atmux"))
        );
        let final_step = if name == "permission" {
            format!(
                "read answer; printf '%s' \"$answer\" > {}; sleep 300; :",
                shell_words::quote(root.join("answered").to_str().unwrap())
            )
        } else {
            "sleep 300; :".into()
        };
        let script = format!("{command}; {hook}; {final_step}");
        let launch = format!(
            "exec env XDG_RUNTIME_DIR={} TMPDIR={} {} -c {}",
            shell_words::quote(root.join("runtime").to_str().unwrap()),
            shell_words::quote(root.join("runtime").to_str().unwrap()),
            shell_words::quote(root.join("home/bin/claude").to_str().unwrap()),
            shell_words::quote(&script)
        );
        let pane = self.tmux(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
            root.join("project").to_str().unwrap(),
            &launch,
        ]);
        self.tmux(&["set-option", "-p", "-t", &pane, "@atmux_status", "waiting"]);
        let pid = self
            .tmux(&["display-message", "-p", "-t", &pane, "#{pane_pid}"])
            .parse::<u32>()
            .unwrap();
        let cwd = root.join("project");
        let id = match name {
            "permission" => "0199a5b7-5560-7abc-8def-012345678901",
            "done" => "0199a5b7-5560-7abc-8def-012345678902",
            _ => "0199a5b7-5560-7abc-8def-012345678903",
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
        let claude = root.join("home/.claude");
        let log = claude
            .join("projects")
            .join(encoded)
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::create_dir_all(claude.join("sessions")).unwrap();
        let text = if name == "done" {
            "JOB DONE job-done: TESTS PASSED https://github.com/org/repo/pull/1"
        } else {
            "Please allow the command."
        };
        let rows = [
            json!({"type":"user","uuid":"human","message":{"role":"user","content":"Implement assigned job within this repo"}}),
            json!({"type":"assistant","uuid":"agent","message":{"role":"assistant","content":[{"type":"text","text":text}]}}),
        ];
        fs::write(
            log,
            rows.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        fs::write(
            claude.join(format!("sessions/{pid}.json")),
            serde_json::to_vec(&json!({"pid":pid,"cwd":cwd,"startedAt":now_ms(),"sessionId":id}))
                .unwrap(),
        )
        .unwrap();
    }
}
impl Drop for DisposableOwner {
    fn drop(&mut self) {
        if let Some(child) = &mut self.web {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output();
        let _ = fs::remove_dir_all(&self.directory);
    }
}
#[derive(Clone, Default)]
struct Fixtures {
    url: Arc<Mutex<String>>,
    jobs: Arc<Mutex<Vec<Value>>>,
    calls: Arc<Mutex<Vec<Value>>>,
}
async fn discovery(State(s): State<Fixtures>) -> Json<Value> {
    let url = s.url.lock().unwrap().clone();
    Json(
        json!({"issuer":url,"token_endpoint":format!("{url}/token"),"device_authorization_endpoint":format!("{url}/device"),"jwks_uri":format!("{url}/keys")}),
    )
}
async fn keys() -> Json<Value> {
    Json(serde_json::from_str(include_str!("fixtures/intake/jwks.json")).unwrap())
}
async fn token(State(s): State<Fixtures>) -> Json<Value> {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("fixture".into());
    let jwt=jsonwebtoken::encode(&header,&json!({"iss":s.url.lock().unwrap().clone(),"sub":"ryan-fixture","azp":"hd-atmux","tenant_id":"hq","identity_type":"USER","aud":["hd-subgraphs","https://hq.herodevs.dev/hd-mcp"],"exp":now_ms()/1000+300}),
        &jsonwebtoken::EncodingKey::from_rsa_der(include_bytes!("fixtures/intake/test-only-key.der"))).unwrap();
    Json(json!({"token_type":"Bearer","access_token":jwt,"refresh_token":"test-refresh"}))
}
async fn hd(State(s): State<Fixtures>, Json(v): Json<Value>) -> Json<Value> {
    s.calls.lock().unwrap().push(v.clone());
    let query = v["query"].as_str().unwrap();
    if query.contains("listJobs") {
        Json(json!({"data":{"tenant":{"listJobs":s.jobs.lock().unwrap().clone()}}}))
    } else if query.contains("renewClaim") {
        let mut jobs = s.jobs.lock().unwrap();
        let job = jobs
            .iter_mut()
            .find(|j| j["id"] == v["variables"]["id"])
            .unwrap();
        assert_eq!(v["variables"]["fence"], job["fenceToken"]);
        assert_eq!(v["variables"]["detail"], 3600);
        job["leaseExpiresAt"] =
            json!(chrono::DateTime::from_timestamp(
            i64::try_from(now_ms() / 1000 + 3600).unwrap(), 0,
        ).unwrap().to_rfc3339());
        Json(json!({"data":{"channelMutations":{"renewClaim":job.clone()}}}))
    } else if query.contains("completeJob") {
        let mut jobs = s.jobs.lock().unwrap();
        let job = jobs
            .iter_mut()
            .find(|j| j["id"] == v["variables"]["id"])
            .unwrap();
        assert_eq!(v["variables"]["fence"], job["fenceToken"]);
        job["jobState"] = json!("COMPLETED");
        Json(json!({"data":{"channelMutations":{"completeJob":job.clone()}}}))
    } else if query.contains("slackMutations") {
        Json(json!({"data":{"slackMutations":{"sendMessage":{"ok":true}}}}))
    } else if query.contains("sendMessage") {
        Json(json!({"data":{"channelMutations":{"sendMessage":{"id":"notify","metadata":{}}}}}))
    } else {
        Json(json!({"errors":[{"message":"unexpected fixture operation"}]}))
    }
}
async fn github(State(s): State<Fixtures>, Json(v): Json<Value>) -> Json<Value> {
    s.calls.lock().unwrap().push(v.clone());
    Json(
        if v["query"].as_str().unwrap().contains("updateProjectV2") {
            json!({"data":{"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"item"}}}})
        } else {
            json!({"data":{"repository":{"pullRequest":{"url":"https://github.com/org/repo/pull/1","state":"OPEN","commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}}}}})
        },
    )
}
async fn llm(State(s): State<Fixtures>, Json(v): Json<Value>) -> Json<Value> {
    s.calls.lock().unwrap().push(v);
    Json(json!({"choices":[{"message":{"content":"{\"action\":\"continue\"}"}}]}))
}

#[tokio::test]
async fn configured_supervisor_answers_verifies_closes_and_archives_disposable_agents() {
    assert!(
        Command::new("tmux")
            .arg("-V")
            .output()
            .unwrap()
            .status
            .success(),
        "tmux required"
    );
    let mut owner = DisposableOwner::new();
    let root = owner.directory.clone();
    fs::create_dir_all(root.join("home/bin")).unwrap();
    fs::create_dir(root.join("project")).unwrap();
    fs::create_dir(root.join("runtime")).unwrap();
    fs::set_permissions(root.join("runtime"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::copy("/bin/sh", root.join("home/bin/claude")).unwrap();
    for name in ["permission", "done", "danger"] {
        owner.create_agent(name);
    }
    let node_token = root.join("node-token");
    fs::write(&node_token, "test-only-node-token").unwrap();
    fs::set_permissions(&node_token, fs::Permissions::from_mode(0o600)).unwrap();
    let listen = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listen.local_addr().unwrap();
    drop(listen);
    let config_path = root.join("owner.toml");
    fs::write(&config_path,format!("profiles=[]\n[general]\nproject_roots=[]\nfavorite_dirs=[]\nswitch_on_launch=false\nrefresh_ms=100\n[node]\nid='fixture'\ntoken_file='{}'\n[registry]\nenabled=true\ndirectory='{}'\n[events]\ninject_hooks=false\ndirectory='{}'\n[web]\nallow_unauthenticated_loopback=true\n",node_token.display(),root.join("registry").display(),root.join("events").display())).unwrap();
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
            .env("HOME", root.join("home"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root.join("runtime"))
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.join("web.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    let machine = MachineConfig {
        id: "fixture".into(),
        label: None,
        url: format!("http://{address}"),
        token_env: None,
        token_file: Some(node_token),
    };
    let remote = RemoteMachine::from_config(&machine).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let sessions = loop {
        if let Ok(page) = remote
            .get_json::<RegistryPage>("/api/v1/registry?wait_ms=0")
            .await
            && page.records.len() == 3
            && page.records.iter().all(|r| r.native.is_some())
            && let Ok(sessions) = remote.get_json::<Overview>("/api/v1/sessions").await
        {
            break sessions.sessions;
        }
        assert!(
            Instant::now() < deadline,
            "owner startup timed out: {}",
            fs::read_to_string(root.join("web.log")).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let permission = sessions.iter().find(|s| s.name == "permission").unwrap();
    let stale = GuardedMessage {
        guard: Guard {
            session_key: permission.session_key.clone().unwrap(),
            instance_id: permission.instance_id.clone(),
            content_hash: "0000000000000000".into(),
            status: "waiting".into(),
        },
        text: "y".into(),
    };
    assert!(
        remote
            .post_json(
                &format!("/api/v1/supervisor/panes/{}/message", permission.pane_id),
                &stale
            )
            .await
            .is_err()
    );
    assert!(
        !root.join("answered").exists(),
        "stale evidence must not send input"
    );
    let danger = sessions.iter().find(|s| s.name == "danger").unwrap();
    let extra = owner.tmux(&[
        "split-window",
        "-d",
        "-t",
        &danger.pane_id,
        "-P",
        "-F",
        "#{pane_id}",
        "sleep 300",
    ]);
    let guard = Guard {
        session_key: danger.session_key.clone().unwrap(),
        instance_id: danger.instance_id.clone(),
        content_hash: danger.content_hash.clone(),
        status: "waiting".into(),
    };
    assert!(
        remote
            .post_json(
                &format!("/api/v1/supervisor/panes/{}/close", danger.pane_id),
                &guard
            )
            .await
            .is_err()
    );
    assert!(
        owner
            .tmux(&["list-panes", "-t", "danger", "-F", "#{pane_id}"])
            .contains(&extra)
    );
    owner.tmux(&["kill-pane", "-t", &extra]);
    let fixtures = Fixtures::default();
    for session in &sessions {
        fixtures.jobs.lock().unwrap().push(json!({"id":format!("message-{}",session.name),"jobId":format!("job-{}",session.name),"channelId":"board","jobState":"IN_PROGRESS","fenceToken":7,
            "leaseExpiresAt":chrono::DateTime::from_timestamp(i64::try_from(now_ms()/1000+800).unwrap(),0).unwrap().to_rfc3339(),
            "metadata":{"session_key":session.session_key,"folder":root.join("project"),"repo_remote":"https://github.com/org/repo","goal":"Fix tests","completion_criteria":"PR open, CI green, tests reported",
                "project_id":"project","project_item_id":"item","require_pr":true,"require_ci":true,"require_tests":true}}));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    *fixtures.url.lock().unwrap() = url.clone();
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/keys", get(keys))
        .route("/token", post(token))
        .route("/graphql", post(hd))
        .route("/github", post(github))
        .route("/v1/chat/completions", post(llm))
        .with_state(fixtures.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client_secret = root.join("client-secret");
    let refresh = root.join("refresh");
    let github_token = root.join("github-token");
    for path in [&client_secret, &refresh, &github_token] {
        fs::write(path, "test-only-secret").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut config = Config::default();
    config.node.id = "coordinator-fixture".into();
    config.node.coordinator_only = true;
    config.profiles.clear();
    config.general.project_roots.clear();
    config.general.favorite_dirs.clear();
    config.general.switch_on_launch = false;
    config.machines = vec![machine];
    config.events = Some(atmux::events::EventsConfig {
        directory: Some(root.join("fleet-events")),
        inject_hooks: false,
        ..Default::default()
    });
    config.registry.enabled = true;
    config.registry.directory = Some(root.join("fleet-registry"));
    config.herodevs = HerodevsConfig {
        graphql_url: format!("{url}/graphql"),
        mcp_url: format!("{url}/mcp"),
        allow_http_hosts: vec!["127.0.0.1".into()],
        auth: AuthConfig {
            issuer: url.clone(),
            client_secret_file: client_secret,
            refresh_token_file: refresh,
            expected_subject: "ryan-fixture".into(),
            allow_http_hosts: vec!["127.0.0.1".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let stop = root.join("STOP");
    config.supervisor = SupervisorConfig {
        enabled: true,
        dry_run: false,
        kill_switch: Some(stop.clone()),
        store_dir: Some(root.join("supervisor")),
        dashboard_url: "https://atmux.example/".into(),
        slack_channel: "private-channel".into(),
        slack_installation_id: "slack-installation".into(),
        job_channels: vec!["board".into()],
        poll_seconds: 1,
        quiet_seconds: 1,
        digest_hour_utc: 23,
        projects: vec![ProjectStatus {
            project_id: "project".into(),
            channel_id: Some("board".into()),
            field_id: "status-field".into(),
            done_option_id: "done-option".into(),
        }],
        github: GithubConfig {
            endpoint: format!("{url}/github"),
            token_file: github_token,
            allow_http_hosts: vec!["127.0.0.1".into()],
        },
        llm: LlmConfig {
            endpoint: format!("{url}/v1"),
            allow_http_hosts: vec!["127.0.0.1".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let control = ControlPlane::start(config).await.unwrap();
    let done_key = sessions
        .iter()
        .find(|s| s.name == "done")
        .unwrap()
        .session_key
        .clone()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let archived = remote
            .get_json::<RegistryPage>("/api/v1/registry?wait_ms=0")
            .await
            .ok()
            .is_some_and(|p| {
                p.records.iter().any(|r| {
                    r.record.session_key == done_key && r.record.state == SessionState::Archived
                })
            });
        let calls = fixtures.calls.lock().unwrap().clone();
        let notified = calls.iter().any(|v| {
            v["query"]
                .as_str()
                .is_some_and(|q| q.contains("slackMutations"))
        });
        let renewed = calls.iter().any(|v| {
            v["query"]
                .as_str()
                .is_some_and(|q| q.contains("renewClaim"))
        });
        if fs::read_to_string(root.join("answered")).ok().as_deref() == Some("y")
            && archived
            && notified
            && renewed
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor scenario timed out; calls={calls:?}, audit={:?}, owner log={}",
            control
                .agent_events(
                    atmux::events::EventQuery {
                        limit: Some(100),
                        ..Default::default()
                    },
                    false
                )
                .await
                .unwrap(),
            fs::read_to_string(root.join("web.log")).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    fs::write(stop, "").unwrap();
    let closed: Value = serde_json::from_slice(
        &fs::read(
            root.join("registry/records")
                .join(format!("{done_key}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        closed["desired_running"], false,
        "completed sessions must not be restored"
    );
    assert_eq!(closed["close_reason"], "user");
    assert_eq!(
        fixtures
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j["jobState"] == "COMPLETED")
            .count(),
        1
    );
    let calls = fixtures.calls.lock().unwrap().clone();
    assert_eq!(
        calls.iter().filter(|v| v.get("messages").is_some()).count(),
        1,
        "destructive permission must never reach Qwen"
    );
    assert!(
        calls
            .iter()
            .any(|v| v.pointer("/variables/input/value/singleSelectOptionId")
                == Some(&json!("done-option")))
    );
    drop(calls);
    let events = control
        .agent_events(
            atmux::events::EventQuery {
                limit: Some(100),
                ..Default::default()
            },
            false,
        )
        .await
        .unwrap();
    for kind in [
        "supervisor.answered",
        "supervisor.completed",
        "supervisor.project_updated",
        "supervisor.closed",
        "supervisor.escalated",
        "supervisor.renewed",
    ] {
        assert!(
            events.events.iter().any(|e| e.event.event_type == kind),
            "missing audit event {kind}"
        );
    }
    let owner_events = remote
        .get_json::<EventPage>("/api/v1/agent-events?limit=100")
        .await
        .unwrap();
    assert!(
        owner_events
            .events
            .iter()
            .any(|e| e.event.event_type == "session.archived" && e.event.session_key == done_key)
    );
    server.abort();
}
