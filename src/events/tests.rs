use super::*;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("atmux-events-{}", tmux::new_session_key().unwrap()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn event() -> AgentEvent {
    let mut event = AgentEvent::node_started("tron").unwrap();
    event.pane = "tron~%7".into();
    event.event_type = "agent.needs_input".into();
    event.reason = Some("permission".into());
    event
}

fn pane_session() -> Session {
    Session {
        name: "fake".into(),
        description: None,
        attached: false,
        windows: 1,
        activity: 0,
        output_activity: 0,
        window_index: 0,
        pane_index: 0,
        pane_id: "%7".into(),
        pane_pid: 123,
        pane_identity: "pane-v1-fake".into(),
        agent_pid: Some(123),
        agent_started_ms: None,
        path: PathBuf::from("/nonexistent/atmux-events-fixture"),
        command: "claude".into(),
        launch_command: String::new(),
        title: String::new(),
        content: "❯ ".into(),
        content_hash: 0,
        agent: AgentKind::Claude,
        profile: "fixture".into(),
        resume_lease: None,
        session_key: Some(tmux::new_session_key().unwrap()),
        systemd_scope: None,
        memory_max_bytes: None,
        status: crate::status::AgentStatus::Waiting,
    }
}

#[tokio::test]
async fn native_and_status_signals_deduplicate_across_multiple_claude_turns() {
    let temp = Temp::new();
    let service = EventService::open(
        EventsConfig {
            directory: Some(temp.0.join("events")),
            ..EventsConfig::default()
        },
        "tron".into(),
        false,
    )
    .unwrap();
    let mut session = pane_session();
    service.observe(std::slice::from_ref(&session));
    session.status = crate::status::AgentStatus::Working;
    service.observe(std::slice::from_ref(&session));
    session.status = crate::status::AgentStatus::Waiting;
    service.observe(std::slice::from_ref(&session));
    let count = service
        .owner
        .read(&EventQuery::default())
        .await
        .unwrap()
        .events
        .len();
    let mut hook = hooks::HookDelivery {
        harness: "claude".into(),
        pane: session.pane_id.clone(),
        parent_pid: 1,
        event: None,
        payload: json!({"hook_event_name":"Stop"}),
    };
    service.ingest_hook(&session, &hook).unwrap();
    service.ingest_hook(&session, &hook).unwrap();
    assert_eq!(
        service
            .owner
            .read(&EventQuery::default())
            .await
            .unwrap()
            .events
            .len(),
        count
    );
    for _ in 0..2 {
        hook.payload = json!({"hook_event_name":"UserPromptSubmit","prompt":"secret"});
        service.ingest_hook(&session, &hook).unwrap();
        hook.payload = json!({"hook_event_name":"Stop"});
        service.ingest_hook(&session, &hook).unwrap();
    }
    let snapshot = service.owner.read(&EventQuery::default()).await.unwrap();
    assert_eq!(
        snapshot
            .events
            .iter()
            .filter(|v| v.event.event_type == "agent.turn_completed")
            .count(),
        3
    );
    assert_eq!(
        snapshot
            .events
            .iter()
            .filter(|v| v.event.event_type == "agent.working")
            .count(),
        3
    );
    assert_eq!(
        snapshot
            .events
            .iter()
            .filter(|v| v.event.event_type == "agent.started")
            .count(),
        1
    );
    hook.payload =
        json!({"hook_event_name":"Notification","notification_type":"permission_prompt"});
    service.ingest_hook(&session, &hook).unwrap();
    let cursor = service
        .owner
        .read(&EventQuery::default())
        .await
        .unwrap()
        .next;
    session.status = crate::status::AgentStatus::Working;
    service.observe(std::slice::from_ref(&session));
    session.status = crate::status::AgentStatus::Waiting;
    service.observe(std::slice::from_ref(&session));
    assert!(
        service
            .owner
            .read(&EventQuery {
                after: Some(cursor),
                ..EventQuery::default()
            })
            .await
            .unwrap()
            .events
            .is_empty()
    );
}

#[test]
fn startup_and_permission_dialog_reasons_are_stable() {
    for content in [
        "Claude Code development channels\n❯ 1. Continue\n  2. Exit\nEnter to confirm",
        "Do you trust the files in this folder?\n❯ 1. Yes, I trust this folder\nEnter to confirm",
    ] {
        assert!(crate::status::startup_prompt(AgentKind::Claude, content));
        assert_eq!(
            crate::status::input_reason(AgentKind::Claude, content),
            Some("startup_prompt")
        );
        assert_eq!(
            crate::status::classify(
                AgentKind::Claude,
                content,
                "",
                "",
                true,
                &crate::config::StatusConfig::default()
            ),
            crate::status::AgentStatus::Waiting
        );
    }
    assert_eq!(
        crate::status::input_reason(AgentKind::Codex, "Allow this command?\nYes, allow"),
        Some("permission")
    );
    assert_eq!(
        crate::status::input_reason(AgentKind::Codex, "Implement this plan?"),
        Some("plan_approval")
    );
}

#[test]
fn envelope_requires_uuidv7_known_types_reasons_and_size() {
    let good = event();
    assert!(good.validate().is_ok());
    let mut bad = good.clone();
    bad.id = "INVALID".into();
    assert!(bad.validate().is_err());
    bad = good.clone();
    bad.event_type = "agent.unknown".into();
    assert!(bad.validate().is_err());
    bad = good.clone();
    bad.reason = Some("unknown".into());
    assert!(bad.validate().is_err());
    bad = good.clone();
    bad.time = "2026-09-30T12:00:00-06:00".into();
    assert!(bad.validate().is_err());
    bad = good.clone();
    bad.summary = Some("x".repeat(2049));
    assert!(bad.validate().is_err());
    bad = good.clone();
    bad.summary = Some("two\nlines".into());
    assert!(bad.validate().is_err());
    bad = good.clone();
    bad.detail = json!({"digest":"x".repeat(MAX_EVENT_BYTES)});
    assert!(bad.bounded_json().is_err());
    bad = good;
    bad.project.remote = Some("https://token@github.com/org/repo?token=x".into());
    assert!(bad.validate().is_err());
}

#[test]
fn remote_urls_strip_credentials_queries_and_fragments() {
    for (raw, clean) in [
        (
            "https://me:secret@github.com/org/repo.git?token=secret#private",
            "https://github.com/org/repo.git",
        ),
        (
            "git@github.com:org/repo.git",
            "ssh://github.com/org/repo.git",
        ),
        (
            "ssh://git:password@example.com/repo",
            "ssh://example.com/repo",
        ),
    ] {
        assert_eq!(credential_free_remote(raw).as_deref(), Some(clean));
    }
    assert!(credential_free_remote("file:///tmp/repo").is_none());
    assert!(credential_free_remote("https://host/\nsecret").is_none());
}

#[test]
fn native_hook_fixtures_map_without_leaking_message_or_tool_bodies() {
    for (harness, payload, kind, reason) in [
        (
            "claude",
            include_str!("../../tests/fixtures/agent-events/claude-notification.json"),
            "agent.needs_input",
            Some("permission"),
        ),
        (
            "codex",
            include_str!("../../tests/fixtures/agent-events/codex-permission.json"),
            "agent.needs_input",
            Some("permission"),
        ),
        (
            "codex",
            include_str!("../../tests/fixtures/agent-events/codex-stop.json"),
            "agent.turn_completed",
            None,
        ),
    ] {
        let delivery = hooks::HookDelivery {
            harness: harness.into(),
            pane: "%7".into(),
            parent_pid: 123,
            event: None,
            payload: serde_json::from_str(payload).unwrap(),
        };
        let mut event = event();
        assert!(hooks::map_hook(&mut event, &delivery));
        assert_eq!(event.event_type, kind);
        assert_eq!(event.reason.as_deref(), reason);
        assert_eq!(event.harness, harness);
        let serialized = String::from_utf8(event.bounded_json().unwrap()).unwrap();
        assert!(!serialized.contains("Raw"));
        assert!(!serialized.contains("secret-token"));
        assert!(!serialized.contains("transcript_path"));
    }
}

#[test]
fn all_supported_hook_mappings_and_ignored_notifications() {
    for harness in ["claude", "codex"] {
        for (name, kind, reason) in [
            ("SessionStart", "agent.started", Some("launch")),
            ("SessionEnd", "agent.exited", None),
            ("UserPromptSubmit", "agent.working", None),
            ("PreCompact", "agent.compacted", None),
        ] {
            let delivery = hooks::HookDelivery {
                harness: harness.into(),
                pane: "%7".into(),
                parent_pid: 123,
                event: None,
                payload: json!({"hook_event_name":name,"trigger":"auto","prompt":"secret"}),
            };
            let mut event = event();
            assert!(hooks::map_hook(&mut event, &delivery));
            assert_eq!(event.event_type, kind);
            assert_eq!(event.reason.as_deref(), reason);
            event.validate().unwrap();
        }
    }
    let mut delivery = hooks::HookDelivery {
        harness: "claude".into(),
        pane: "%7".into(),
        parent_pid: 1,
        event: None,
        payload: json!({"hook_event_name":"Notification", "notification_type":"idle_prompt"}),
    };
    let mut e = event();
    assert!(hooks::map_hook(&mut e, &delivery));
    assert_eq!(e.reason.as_deref(), Some("idle_prompt"));
    delivery.payload["notification_type"] = json!("auth_success");
    assert!(!hooks::map_hook(&mut e, &delivery));
    delivery.harness = "codex".into();
    assert!(!hooks::map_hook(&mut e, &delivery));
}

#[test]
fn injection_is_valid_process_scoped_json_and_toml_and_optional() {
    let binary = std::path::Path::new("/tmp/atmux with spaces");
    let claude = hooks::injection_args("claude", binary).unwrap();
    let settings: Value = serde_json::from_str(&claude[1]).unwrap();
    assert!(settings["hooks"]["Notification"].is_array());
    assert_eq!(settings.as_object().unwrap().len(), 1);
    let codex = hooks::injection_args("codex", binary).unwrap();
    for override_value in codex.windows(2).filter(|v| v[0] == "-c").map(|v| &v[1]) {
        let config: toml::Value = toml::from_str(override_value).unwrap();
        assert!(config.is_table());
    }
    assert!(codex.iter().any(|v| v == "--dangerously-bypass-hook-trust"));
    assert!(!codex.iter().any(|v| v.contains("Notification")));
    let original = vec![
        "env".into(),
        "CLAUDE_CONFIG_DIR=/tmp/store".into(),
        "/tmp/claude-max".into(),
        "--resume".into(),
        "id".into(),
    ];
    assert_eq!(inject_command(original.clone(), false).unwrap(), original);
    let injected = inject_command(original, true).unwrap();
    assert_eq!(injected[3], "--settings");
    assert!(injected.contains(&"--resume".into()));
    assert_eq!(inject_command(injected.clone(), true).unwrap(), injected);
    let mut config: crate::config::Config = toml::from_str(crate::config::DEFAULT_CONFIG).unwrap();
    let original = config.profiles.clone();
    configure_profiles(&mut config).unwrap();
    assert_eq!(config.profiles, original);
    config.events = Some(EventsConfig::default());
    configure_profiles(&mut config).unwrap();
    let first = config.profiles.clone();
    configure_profiles(&mut config).unwrap();
    assert_eq!(config.profiles, first);
}

#[tokio::test]
async fn spool_paging_filtered_cursors_and_long_poll_do_not_lose_wakeups() {
    let temp = Temp::new();
    let log = Arc::new(EventLog::open(temp.0.join("log"), EventsConfig::default()).unwrap());
    let a = event();
    log.append(a.clone()).unwrap();
    log.append(a.clone()).unwrap();
    let page = log.read(&EventQuery::default()).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert!(!page.reset);
    let cursor = page.next;
    let reader = log.clone();
    let after = cursor.clone();
    let wait = tokio::spawn(async move {
        reader
            .read(&EventQuery {
                after: Some(after),
                wait: Some(2),
                ..EventQuery::default()
            })
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    log.append(event()).unwrap();
    assert_eq!(wait.await.unwrap().events.len(), 1);
    let filtered = log
        .read(&EventQuery {
            after: Some(cursor),
            types: Some("agent.working".into()),
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert!(filtered.events.is_empty());
    assert!(filtered.next.ends_with(":2"));
    assert!(
        log.read(&EventQuery {
            after: Some("bad".into()),
            ..EventQuery::default()
        })
        .await
        .is_err()
    );
    assert!(
        log.read(&EventQuery {
            wait: Some(31),
            ..EventQuery::default()
        })
        .await
        .is_err()
    );
    assert!(
        log.read(&EventQuery {
            limit: Some(101),
            ..EventQuery::default()
        })
        .await
        .is_err()
    );
    let future = log
        .read(&EventQuery {
            after: Some(format!("{}:999", filtered.epoch)),
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert!(future.reset);
}

#[tokio::test]
async fn spool_restarts_rotation_retention_reset_and_partial_tail_recovery() {
    use std::io::Write as _;
    let temp = Temp::new();
    let path = temp.0.join("log");
    let config = EventsConfig {
        max_bytes: 256 * 1024,
        segment_bytes: 72 * 1024,
        retention_seconds: 1,
        ..EventsConfig::default()
    };
    let log = EventLog::open(path.clone(), config.clone()).unwrap();
    let mut first = None;
    for _ in 0..15 {
        let mut event = event();
        event.detail = json!({"digest":"x".repeat(48 * 1024)});
        log.append(event).unwrap();
        if first.is_none() {
            first = Some(log.read(&EventQuery::default()).await.unwrap().next);
        }
    }
    let old = first.unwrap();
    let page = log
        .read(&EventQuery {
            after: Some(old),
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert!(page.reset);
    assert!(page.events.len() <= 5);
    let size: u64 = fs::read_dir(&path)
        .unwrap()
        .map(|v| v.unwrap().metadata().unwrap().len())
        .sum();
    assert!(size < config.max_bytes + 8192);
    let cursor = page.next.clone();
    drop(log);
    let mut paths: Vec<_> = fs::read_dir(&path)
        .unwrap()
        .map(|v| v.unwrap().path())
        .filter(|v| v.extension().is_some_and(|v| v == "jsonl"))
        .collect();
    paths.sort();
    fs::OpenOptions::new()
        .append(true)
        .open(paths.last().unwrap())
        .unwrap()
        .write_all(b"{\"seq\":")
        .unwrap();
    let log = EventLog::open(path.clone(), config.clone()).unwrap();
    assert!(
        log.read(&EventQuery {
            after: Some(cursor.clone()),
            ..EventQuery::default()
        })
        .await
        .unwrap()
        .events
        .is_empty()
    );
    log.append(event()).unwrap();
    drop(log);
    for path in paths {
        if path.exists() {
            fs::File::open(path)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(5))
                .unwrap();
        }
    }
    let log = EventLog::open(path.clone(), config.clone()).unwrap();
    assert!(
        log.read(&EventQuery {
            after: Some(cursor),
            ..EventQuery::default()
        })
        .await
        .unwrap()
        .reset
    );
    let epoch = log.read(&EventQuery::default()).await.unwrap().epoch;
    drop(log);
    fs::remove_dir_all(&path).unwrap();
    let log = EventLog::open(path, config).unwrap();
    let reset = log
        .read(&EventQuery {
            after: Some(format!("{epoch}:0")),
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert!(reset.reset);
    assert_ne!(reset.epoch, epoch);
}

#[tokio::test]
async fn federation_checkpoints_survive_restart_and_replay_is_deduplicated() {
    let temp = Temp::new();
    let config = EventsConfig {
        directory: Some(temp.0.join("home")),
        ..EventsConfig::default()
    };
    let service = EventService::open(config.clone(), "home".into(), true).unwrap();
    let epoch = tmux::new_session_key().unwrap();
    let mut remote = event();
    remote.machine = "remote".into();
    remote.pane = "remote~%8".into();
    let page = EventPage {
        epoch: epoch.clone(),
        next: format!("{epoch}:1"),
        reset: false,
        events: vec![StoredEvent {
            seq: 1,
            event: remote,
        }],
    };
    service.import_page("remote", &page).unwrap();
    service.import_page("remote", &page).unwrap();
    assert_eq!(
        service
            .fleet
            .read(&EventQuery::default())
            .await
            .unwrap()
            .events
            .len(),
        1
    );
    drop(service);
    let service = EventService::open(config, "home".into(), true).unwrap();
    service.import_page("remote", &page).unwrap();
    assert_eq!(
        service
            .fleet
            .read(&EventQuery::default())
            .await
            .unwrap()
            .events
            .len(),
        1
    );
    assert!(service.import_page("imposter", &page).is_err());
}

#[derive(Default)]
struct FakeProducer {
    values: Mutex<Vec<Publication>>,
    fail: std::sync::atomic::AtomicBool,
}
type Publication = (String, Vec<u8>, Vec<u8>);
impl sink::Producer for FakeProducer {
    fn publish<'a>(
        &'a self,
        topic: &'a str,
        key: &'a [u8],
        value: &'a [u8],
    ) -> sink::PublishFuture<'a> {
        Box::pin(async move {
            if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
                anyhow::bail!("fake offline");
            }
            self.values
                .lock()
                .unwrap()
                .push((topic.into(), key.to_vec(), value.to_vec()));
            Ok(())
        })
    }
}
#[tokio::test]
async fn sink_retries_without_advancing_and_wraps_the_verified_platform_contract() {
    let temp = Temp::new();
    let log = EventLog::open(temp.0.join("log"), EventsConfig::default()).unwrap();
    let event = event();
    let key = event.session_key.clone();
    log.append(event).unwrap();
    let producer = FakeProducer::default();
    let config = RedpandaConfig::default();
    producer
        .fail
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        sink::publish_page(&log, &config, &producer, None)
            .await
            .is_err()
    );
    producer
        .fail
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let next = sink::publish_page(&log, &config, &producer, None)
        .await
        .unwrap();
    sink::publish_page(&log, &config, &producer, Some(next))
        .await
        .unwrap();
    let values = producer.values.lock().unwrap();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].0, "atmux.agent.events.v1");
    assert_eq!(values[0].1, key.as_bytes());
    let value: Value = serde_json::from_slice(&values[0].2).unwrap();
    assert_eq!(value["envelopeVersion"], 2);
    assert_eq!(value["securityContext"]["platform"], "SYSTEM");
    assert_eq!(value["securityContext"]["tenantId"], config.tenant_id);
    assert_eq!(value["eventPayload"]["tenantId"], config.tenant_id);
    assert_eq!(value["eventPayload"]["schema"], SCHEMA);
    assert!(value["securityContext"]["token"].is_null());
}
