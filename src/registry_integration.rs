//! Registry lifecycle -> A1 owner events and A2 full search snapshots.
//! Installed before the initial owner scan; remote imports publish search only.
use crate::{
    control::ControlPlane,
    events::{AgentEvent, EventProject, SCHEMA},
    registry::{RegistryChange, RegistrySnapshot, SessionRecord, SessionState},
    session_search::SearchChange,
    summarizer::DigestRecord,
    tmux,
};
use anyhow::{Context as _, Result};
use chrono::{SecondsFormat, Utc};
use std::sync::Arc;

pub(crate) fn install(control: &ControlPlane, events_enabled: bool) -> Result<()> {
    let Ok(registry) = control.registry() else {
        return Ok(());
    };
    let owner = control.local_id().to_owned();
    let weak = control.downgrade();
    registry.set_change_sink(Arc::new(move |change| {
        let Some(control) = weak.upgrade() else {
            return Ok(());
        };
        if events_enabled && change.current.record.machine == owner {
            emit_transitions(&control, change)?;
        }
        if searchable_change(change) {
            let current = &change.current;
            let cached = control.session_digest_record(&current.record.session_key);
            let snapshot = DigestRecord::registry_snapshot(
                &current.record,
                &format!("{}~{}", current.record.machine, current.pane_id),
                cached,
            );
            let timestamp = snapshot.next_registry_search_timestamp(change.timestamp_ms);
            // Archival is an update, retaining a complete searchable snapshot.
            control.publish_digest_search(&snapshot, SearchChange::Updated, timestamp)?;
        }
        Ok(())
    }))
}

fn searchable_change(change: &RegistryChange) -> bool {
    change.previous.as_ref().is_none_or(|previous| {
        let previous = &previous.record;
        let current = &change.current.record;
        previous.state != current.state
            || previous.machine != current.machine
            || previous.name != current.name
            || previous.description != current.description
            || previous.description_source != current.description_source
            || previous.cwd != current.cwd
            || previous.project != current.project
            || previous.harness != current.harness
            || previous.profile != current.profile
    })
}

fn emit_transitions(control: &ControlPlane, change: &RegistryChange) -> Result<()> {
    let Some(previous) = &change.previous else {
        return Ok(()); // Startup replay indexes history without inventing events.
    };
    let current = &change.current;
    // The registry runs before the status observer. Both use the durable event
    // spool's process-generation deduplication, also shared by native SessionEnd.
    if previous.record.machine == current.record.machine
        && previous.record.state == SessionState::Running
        && (current.record.state != SessionState::Running
            || current.agent_pid != previous.agent_pid
            || current.instance_id != previous.instance_id)
    {
        let mut ended = previous.clone();
        ended.record.state = SessionState::Exited;
        control.emit_agent_event(event(&ended, "agent.exited", change.timestamp_ms)?)?;
    }
    if current.record.state == SessionState::Closed
        && previous.record.closed_ms != current.record.closed_ms
    {
        control.emit_agent_event(event(current, "session.closed", change.timestamp_ms)?)?;
    }
    if current.record.state == SessionState::Archived
        && previous.record.archived_ms != current.record.archived_ms
    {
        control.emit_agent_event(event(current, "session.archived", change.timestamp_ms)?)?;
    }
    Ok(())
}

fn event(snapshot: &RegistrySnapshot, kind: &str, timestamp_ms: u64) -> Result<AgentEvent> {
    let record = &snapshot.record;
    let timestamp_ms = match kind {
        "session.closed" => record.closed_ms.unwrap_or(timestamp_ms),
        "session.archived" => record.archived_ms.unwrap_or(timestamp_ms),
        _ => timestamp_ms,
    };
    let time = chrono::DateTime::<Utc>::from_timestamp_millis(i64::try_from(timestamp_ms)?)
        .context("invalid registry lifecycle time")?;
    let event = AgentEvent {
        schema: SCHEMA.into(),
        id: tmux::new_session_key()?,
        time: time.to_rfc3339_opts(SecondsFormat::Millis, true),
        machine: record.machine.clone(),
        session_key: record.session_key.clone(),
        pane: format!("{}~{}", record.machine, snapshot.pane_id),
        instance_id: attribute(&snapshot.instance_id),
        session_name: attribute(&record.name),
        harness: record.harness.clone(),
        profile: attribute(&record.profile),
        model: record.model.clone(),
        cwd: attribute(&record.cwd),
        project: EventProject {
            remote: record.project.remote.clone(),
            branch: record.project.branch.clone(),
            root: (!record.project.root.is_empty()).then(|| record.project.root.clone()),
        },
        event_type: kind.into(),
        reason: None,
        summary: None,
        detail: serde_json::json!({
            "state": state_name(record),
            "agent_pid": snapshot.agent_pid,
            "closed_ms": record.closed_ms,
            "archived_ms": record.archived_ms,
            "archive_bundle_id": if kind == "session.archived" {
                record.bundle.as_ref().map(|bundle| bundle.id.as_str())
            } else { None },
        }),
    };
    event.validate()?;
    Ok(event)
}
fn state_name(record: &SessionRecord) -> &'static str {
    match record.state {
        SessionState::Running => "running",
        SessionState::Exited => "exited",
        SessionState::Closed => "closed",
        SessionState::Archived => "archived",
    }
}
fn attribute(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        control::{test_control_with_config, test_session},
        events::{EventQuery, EventsConfig},
        registry::{NativeIdentity, RegistryPage, StoredRecord},
        session_search::{ChangeType, HQ_TENANT, SearchSnapshot, search_publication},
        status::AgentKind,
        tmux::Session,
    };
    use std::{fs, path::PathBuf};

    struct Fixture {
        directory: PathBuf,
        config: Config,
    }
    impl Fixture {
        fn new(search: bool) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "atmux-registry-integration-{}",
                tmux::new_session_key().unwrap()
            ));
            fs::create_dir(&directory).unwrap();
            let directory = directory.canonicalize().unwrap();
            let mut config = Config::default();
            config.node.id = "fixture".into();
            config.node.coordinator_only = search;
            config.registry.enabled = true;
            config.registry.directory = Some(directory.join("registry"));
            config.events = Some(EventsConfig {
                directory: Some(directory.join("events")),
                inject_hooks: false,
                ..EventsConfig::default()
            });
            if search {
                config.summaries.enabled = true;
                config.summaries.endpoint = "http://127.0.0.1:9/v1".into();
                config.summaries.allow_http_hosts = vec!["127.0.0.1".into()];
                config.summaries.store_dir = Some(directory.join("summaries"));
                config.summaries.search_tenant_id = Some(HQ_TENANT.into());
            }
            Self { directory, config }
        }
        fn control(&self) -> ControlPlane {
            test_control_with_config(&[], self.config.clone())
        }
        fn session(&self) -> Session {
            let mut session = test_session("Registry fixture", "%7", "working");
            session.session_key = Some(tmux::new_session_key().unwrap());
            session.pane_identity = "pane-v1-fixture".into();
            // pane_pid remains zero: synthetic fixtures cannot inspect real tmux.
            session.path = self.directory.clone();
            session.description = Some("A searchable session without a digest".into());
            session.description_source = Some("user".into());
            session
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
    async fn lifecycle_events(control: &ControlPlane) -> Vec<AgentEvent> {
        control
            .agent_events(EventQuery::default(), true)
            .await
            .unwrap()
            .events
            .into_iter()
            .map(|stored| stored.event)
            .filter(|event| {
                matches!(
                    event.event_type.as_str(),
                    "agent.exited" | "session.closed" | "session.archived"
                )
            })
            .collect()
    }
    fn document(control: &ControlPlane, key: &str) -> SearchSnapshot {
        let cached = control.session_digest_record(key).unwrap();
        let timestamp = watermark(&cached);
        let publication =
            search_publication(&cached, HQ_TENANT, SearchChange::Updated, timestamp).unwrap();
        assert_eq!(
            publication.envelope.event_payload.change_type,
            ChangeType::Updated
        );
        publication.envelope.event_payload.current.unwrap()
    }
    fn watermark(record: &DigestRecord) -> u64 {
        serde_json::to_value(record).unwrap()["search_updated_at_ms"]
            .as_u64()
            .unwrap()
    }

    #[tokio::test]
    async fn registry_first_exit_close_archive_have_final_envelopes_and_are_emitted_once() {
        let fixture = Fixture::new(false);
        let control = fixture.control();
        let registry = control.registry().unwrap();
        let session = fixture.session();
        let key = session.session_key.clone().unwrap();
        registry
            .observe(std::slice::from_ref(&session), 1000)
            .unwrap();
        control.apply_refresh(vec![session.clone()]);
        registry.observe(&[], 2000).unwrap();
        control.apply_refresh(vec![]); // Status observer sees the same exit second.
        registry.observe(&[], 3000).unwrap();
        let events = lifecycle_events(&control).await;
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["agent.exited", "session.closed", "session.archived"]
        );
        let archived = registry.get(&key).unwrap().unwrap();
        for (event, state) in events.iter().zip(["exited", "closed", "archived"]) {
            event.validate().unwrap();
            assert_eq!(event.machine, "fixture");
            assert_eq!(event.session_key, key);
            assert_eq!(event.pane, "fixture~%7");
            assert_eq!(event.instance_id, session.pane_identity);
            assert_eq!(event.session_name, session.name);
            assert_eq!(event.harness, "codex");
            assert_eq!(event.profile, session.profile);
            assert_eq!(event.cwd, session.path.to_str().unwrap());
            assert_eq!(event.detail["state"], state);
            assert_eq!(event.detail["agent_pid"], 100);
        }
        assert_eq!(events[1].time, "1970-01-01T00:00:02.000Z");
        assert_eq!(
            events[2].detail["archive_bundle_id"],
            archived.bundle.unwrap().id
        );
    }

    #[tokio::test]
    async fn pidless_scan_during_close_does_not_reopen_or_emit_a_second_exit() {
        for intentional in [true, false] {
            let fixture = Fixture::new(false);
            let control = fixture.control();
            let registry = control.registry().unwrap();
            let mut session = fixture.session();
            let key = session.session_key.clone().unwrap();
            registry
                .observe(std::slice::from_ref(&session), 1000)
                .unwrap();
            control.apply_refresh(vec![session.clone()]);
            if intentional {
                registry
                    .record_close(std::slice::from_ref(&key), 2000)
                    .unwrap();
            }
            // A banner can still identify the harness after its process has
            // exited, between the pre-close scan and the monitor's next scan.
            session.agent_pid = None;
            registry
                .observe(std::slice::from_ref(&session), 2100)
                .unwrap();
            control.apply_refresh(vec![session]);
            registry.observe(&[], 2200).unwrap();
            control.apply_refresh(vec![]);
            registry.observe(&[], 2300).unwrap();
            let events = lifecycle_events(&control).await;
            assert_eq!(
                events
                    .iter()
                    .map(|e| e.event_type.as_str())
                    .collect::<Vec<_>>(),
                ["agent.exited", "session.closed", "session.archived"],
                "intentional={intentional}: {events:#?}"
            );
            assert_eq!(events[0].detail["agent_pid"], 100);
            if intentional {
                assert_eq!(events[1].detail["closed_ms"], 2000);
                assert_eq!(registry.get(&key).unwrap().unwrap().closed_ms, Some(2000));
            }
        }
    }

    #[tokio::test]
    async fn close_retries_preserve_first_close_and_archived_generation() {
        let fixture = Fixture::new(false);
        let control = fixture.control();
        let registry = control.registry().unwrap();
        let session = fixture.session();
        let key = session.session_key.clone().unwrap();
        registry.observe(&[session], 1000).unwrap();
        registry
            .record_close(&[key.clone(), key.clone()], 2000)
            .unwrap();
        registry
            .record_close(std::slice::from_ref(&key), 2100)
            .unwrap();
        registry.observe(&[], 2200).unwrap();
        registry
            .record_close(std::slice::from_ref(&key), 2300)
            .unwrap();
        let record = registry.get(&key).unwrap().unwrap();
        assert_eq!(record.state, SessionState::Archived);
        assert_eq!(record.closed_ms, Some(2000));
        let events = lifecycle_events(&control).await;
        assert_eq!(
            events
                .iter()
                .map(|e| e.event_type.as_str())
                .collect::<Vec<_>>(),
            ["agent.exited", "session.closed", "session.archived"]
        );
    }

    #[tokio::test]
    async fn native_or_scan_first_exit_deduplicates_but_a_relaunch_has_its_own_exit() {
        let fixture = Fixture::new(false);
        let control = fixture.control();
        let registry = control.registry().unwrap();
        let mut session = fixture.session();
        registry
            .observe(std::slice::from_ref(&session), 1000)
            .unwrap();
        control.apply_refresh(vec![session.clone()]);
        // Native SessionEnd uses this owner emission path before the next scan.
        control
            .emit_agent_event(
                AgentEvent::from_session("fixture", &session, "agent.exited", None).unwrap(),
            )
            .unwrap();
        session.agent = AgentKind::Other;
        session.agent_pid = None;
        registry
            .observe(std::slice::from_ref(&session), 2000)
            .unwrap();
        control.apply_refresh(vec![session.clone()]);
        assert_eq!(lifecycle_events(&control).await.len(), 1);
        session.agent = AgentKind::Codex;
        session.agent_pid = Some(101);
        registry
            .observe(std::slice::from_ref(&session), 3000)
            .unwrap();
        control.apply_refresh(vec![session.clone()]);
        control.apply_refresh(vec![]); // This time the status observer wins.
        registry.observe(&[], 4000).unwrap();
        let events = lifecycle_events(&control).await;
        let exits = events
            .iter()
            .filter(|event| event.event_type == "agent.exited")
            .collect::<Vec<_>>();
        assert_eq!(exits.len(), 2);
        assert_eq!(exits[0].detail["agent_pid"], 100);
        assert_eq!(exits[1].detail["agent_pid"], 101);
        assert_eq!(events.len(), 4);
        // The ledger survives reopening; a replay with a new UUID is still a no-op.
        drop(registry);
        drop(control);
        let reopened = fixture.control();
        reopened
            .emit_agent_event(
                AgentEvent::from_session("fixture", &session, "agent.exited", None).unwrap(),
            )
            .unwrap();
        assert_eq!(lifecycle_events(&reopened).await.len(), 4);
    }

    #[tokio::test]
    async fn no_digest_sessions_publish_rename_exit_and_archive_without_heartbeat_noise() {
        let fixture = Fixture::new(true);
        let control = fixture.control();
        let registry = control.registry().unwrap();
        let mut session = fixture.session();
        let key = session.session_key.clone().unwrap();
        registry
            .observe(std::slice::from_ref(&session), 1000)
            .unwrap();
        let first = document(&control, &key);
        assert_eq!(first.title, session.name);
        assert_eq!(first.description, session.description.clone().unwrap());
        assert!(first.digest.is_empty());
        assert_eq!(first.state, "running");
        let initial = watermark(&control.session_digest_record(&key).unwrap());
        registry
            .observe(std::slice::from_ref(&session), 32_000)
            .unwrap();
        assert_eq!(
            watermark(&control.session_digest_record(&key).unwrap()),
            initial
        );
        session.name = "Renamed before summary".into();
        registry
            .observe(std::slice::from_ref(&session), 33_000)
            .unwrap();
        assert_eq!(document(&control, &key).title, session.name);
        session.agent = AgentKind::Other;
        session.agent_pid = None;
        registry
            .observe(std::slice::from_ref(&session), 34_000)
            .unwrap();
        assert_eq!(document(&control, &key).state, "exited");
        let before_archive = watermark(&control.session_digest_record(&key).unwrap());
        registry.observe(&[], 35_000).unwrap();
        let archived = document(&control, &key);
        assert_eq!(archived.state, "archived");
        assert!(archived.archived_at.is_some());
        assert!(archived.digest.is_empty());
        // Closed and archived can share a wall-clock millisecond; neither loses.
        assert!(watermark(&control.session_digest_record(&key).unwrap()) >= before_archive + 2);
    }

    #[tokio::test]
    async fn cached_digest_survives_archive_with_authoritative_metadata_and_ordering() {
        let fixture = Fixture::new(true);
        let control = fixture.control();
        let registry = control.registry().unwrap();
        let mut session = fixture.session();
        let key = session.session_key.clone().unwrap();
        let cached: DigestRecord = serde_json::from_value(serde_json::json!({
            "session_key": key, "machine":"old-owner", "pane":"old-owner~%3",
            "name":"old name", "description":"generated description", "title":"Cached model title",
            "digest":"Preserved rolling goal, decisions and remaining work.", "digest_version":2,
            "digest_updated_at":1, "transcript_hash":"fixture", "pane_hash":"fixture", "last_entry":null,
            "last_attempt":0, "last_seen":1, "created_at":1, "checked_at":0,
            "project_remote":null, "project_branch":null, "cwd":"/old", "harness":"claude",
            "profile":"old", "state":"working"
        })).unwrap();
        control
            .publish_digest_search(&cached, SearchChange::Created, 1000)
            .unwrap();
        session.description = Some("Automatic description".into());
        session.description_source = Some("auto".into());
        registry
            .observe(std::slice::from_ref(&session), 2000)
            .unwrap();
        assert_eq!(
            registry
                .get(&key)
                .unwrap()
                .unwrap()
                .description_source
                .as_deref(),
            Some("auto")
        );
        assert_eq!(
            document(&control, &key).description,
            "Automatic description"
        );
        session.description = None;
        session.description_source = Some("user".into());
        registry
            .observe(std::slice::from_ref(&session), 2500)
            .unwrap();
        registry.observe(&[], 3000).unwrap();
        let archived = document(&control, &key);
        assert_eq!(archived.title, cached.title);
        assert_eq!(archived.digest, cached.digest);
        assert!(archived.description.is_empty());
        assert_eq!(archived.machine, "fixture");
        assert_eq!(archived.harness, "codex");
        assert_eq!(archived.profile, "Default");
        assert_eq!(archived.cwd, session.path.to_str().unwrap());
        assert_eq!(archived.state, "archived");
        assert!(
            !control
                .publish_digest_search(&cached, SearchChange::Updated, 2000)
                .unwrap()
        );
        assert_eq!(document(&control, &key), archived);
    }

    #[tokio::test]
    async fn remote_registry_snapshots_are_searchable_and_never_emit_owner_events_or_native_identity()
     {
        let fixture = Fixture::new(true);
        let control = fixture.control();
        let registry = control.registry().unwrap();
        let key = tmux::new_session_key().unwrap();
        let mut stored = StoredRecord {
            record: SessionRecord {
                session_key: key.clone(),
                machine: "remote".into(),
                name: "Remote history".into(),
                description: Some("Remote project".into()),
                harness: "claude".into(),
                profile: "max".into(),
                cwd: "/project".into(),
                created_ms: 1000,
                last_seen_ms: 2000,
                ..SessionRecord::default()
            },
            pane_id: "%9".into(),
            instance_id: "pane-v1-remote".into(),
            agent_pid: Some(321),
            native: Some(NativeIdentity {
                config_root: "/private/native".into(),
                session_id: "private-native-id".into(),
                log_path: "/private/native/log.jsonl".into(),
            }),
            ..StoredRecord::default()
        };
        let page = |stored| RegistryPage {
            cursor: String::new(),
            reset: false,
            more: false,
            records: vec![stored],
        };
        registry
            .import_page("remote", &page(stored.clone()))
            .unwrap();
        assert_eq!(document(&control, &key).machine, "remote");
        stored.record.state = SessionState::Archived;
        stored.record.closed_ms = Some(3000);
        stored.record.archived_ms = Some(3000);
        registry.import_page("remote", &page(stored)).unwrap();
        let archived = document(&control, &key);
        assert_eq!(archived.state, "archived");
        let json = serde_json::to_string(&archived).unwrap();
        assert!(!json.contains("private-native"));
        assert!(!json.contains("config_root"));
        assert!(lifecycle_events(&control).await.is_empty());
    }

    #[tokio::test]
    async fn enabling_search_replays_existing_archives_without_inventing_events() {
        let mut fixture = Fixture::new(false);
        let session = fixture.session();
        let key = session.session_key.clone().unwrap();
        {
            let control = fixture.control();
            let registry = control.registry().unwrap();
            registry.observe(&[session], 1000).unwrap();
            registry.observe(&[], 2000).unwrap();
            assert_eq!(lifecycle_events(&control).await.len(), 3);
        }
        fixture.config.node.coordinator_only = true;
        fixture.config.summaries.enabled = true;
        fixture.config.summaries.endpoint = "http://127.0.0.1:9/v1".into();
        fixture.config.summaries.allow_http_hosts = vec!["127.0.0.1".into()];
        fixture.config.summaries.store_dir = Some(fixture.directory.join("summaries"));
        fixture.config.summaries.search_tenant_id = Some(HQ_TENANT.into());
        let control = fixture.control();
        let archived = document(&control, &key);
        assert_eq!(archived.state, "archived");
        assert!(archived.digest.is_empty());
        assert_eq!(lifecycle_events(&control).await.len(), 3);
    }

    #[tokio::test]
    async fn restored_foreign_key_does_not_try_to_emit_the_previous_owners_exit() {
        let fixture = Fixture::new(true);
        let control = fixture.control();
        let registry = control.registry().unwrap();
        let session = fixture.session();
        let key = session.session_key.clone().unwrap();
        registry
            .import_page(
                "remote",
                &RegistryPage {
                    cursor: String::new(),
                    reset: false,
                    more: false,
                    records: vec![StoredRecord {
                        record: SessionRecord {
                            session_key: key.clone(),
                            machine: "remote".into(),
                            name: "Remote running".into(),
                            harness: "codex".into(),
                            created_ms: 1000,
                            last_seen_ms: 1000,
                            ..SessionRecord::default()
                        },
                        pane_id: "%9".into(),
                        instance_id: "pane-v1-remote".into(),
                        agent_pid: Some(321),
                        ..StoredRecord::default()
                    }],
                },
            )
            .unwrap();
        registry.observe(&[session], 2000).unwrap();
        assert_eq!(document(&control, &key).machine, "fixture");
        assert_eq!(document(&control, &key).state, "running");
        assert!(lifecycle_events(&control).await.is_empty());
        registry.observe(&[], 3000).unwrap();
        assert_eq!(lifecycle_events(&control).await.len(), 3);
    }
}
