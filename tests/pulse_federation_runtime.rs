#![cfg(feature = "pulse")]

use std::{path::PathBuf, sync::Arc};

use atmux::pulse::{
    Account, AccountId, CollectionOutcome, Instant, Machine, MachineName, Percent, Profile,
    ProfileName, ProfileOrigin, QuotaWindow, QuotaWindowKind, RefreshPolicy, UsageSnapshot, Vendor,
    federation::{FederatedPulseRow, FederatedRecord, OpaqueCursor, PulseOrigin},
    store::{SqliteStore, Store},
};

fn account(value: i64) -> AccountId {
    AccountId::new(value).expect("account")
}

fn machine(value: &str) -> MachineName {
    MachineName::new(value).expect("machine")
}

fn profile(value: &str) -> ProfileName {
    ProfileName::new(value).expect("profile")
}

fn instant(value: i64) -> Instant {
    Instant::from_epoch_millis(value).expect("instant")
}

fn temp_database(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "atmux-pulse-federation-{name}-{}-{}",
        std::process::id(),
        Instant::now().epoch_millis()
    ));
    std::fs::create_dir(&directory).expect("private federation test directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .expect("protect federation test directory");
    }
    directory.join("pulse.sqlite3")
}

fn remove_database(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    if let Some(directory) = path.parent() {
        let _ = std::fs::remove_dir(directory);
    }
}

async fn seed_local(store: &dyn Store) {
    store
        .upsert_account(Account {
            id: account(7),
            identity: "operator@example.test".to_owned(),
            display_name: None,
        })
        .await
        .expect("account");
    store
        .upsert_machine(Machine {
            account_id: account(7),
            name: machine("tron"),
            first_seen: instant(1),
            last_seen: instant(2),
        })
        .await
        .expect("local machine");
    store
        .upsert_profile(Profile {
            account_id: account(7),
            name: profile("claude"),
            vendor: Vendor::AnthropicOauth,
            config_dir: Some(PathBuf::from("/private/local-claude")),
            poll_interval_minutes: 15,
            monthly_budget_usd: None,
            api_key_env: None,
            api_key_file: None,
            refresh: RefreshPolicy::InMemory,
            hidden: false,
            origin: ProfileOrigin::Local,
        })
        .await
        .expect("local profile");
}

fn origin(source: &str) -> PulseOrigin {
    let source = machine(source);
    PulseOrigin {
        machine: source.clone(),
        path: vec![source],
    }
}

fn remote_records(last_seen: i64) -> Vec<FederatedRecord> {
    vec![
        FederatedRecord {
            key: "a/max".to_owned(),
            origin: origin("max"),
            row: FederatedPulseRow::Machine(Machine {
                account_id: account(7),
                name: machine("max"),
                first_seen: instant(10),
                last_seen: instant(last_seen),
            }),
        },
        FederatedRecord {
            key: "b/claude".to_owned(),
            origin: origin("max"),
            row: FederatedPulseRow::Profile(Profile {
                account_id: account(7),
                name: profile("claude"),
                vendor: Vendor::AnthropicOauth,
                config_dir: None,
                poll_interval_minutes: 15,
                monthly_budget_usd: None,
                api_key_env: None,
                api_key_file: None,
                refresh: RefreshPolicy::Never,
                hidden: false,
                origin: ProfileOrigin::Reported,
            }),
        },
        FederatedRecord {
            key: "c/00000000000000000001".to_owned(),
            origin: origin("max"),
            row: FederatedPulseRow::Usage(UsageSnapshot {
                account_id: account(7),
                profile: profile("claude"),
                machine: machine("max"),
                vendor: Vendor::AnthropicOauth,
                windows: vec![QuotaWindow {
                    kind: QuotaWindowKind::FiveHour,
                    used_percent: Percent::new(20.0).expect("percent"),
                    resets_at: instant(100_000),
                }],
                outcome: CollectionOutcome::Success,
                polled_at: instant(20_000),
                reporter_version: Some("max-test".to_owned()),
            }),
        },
    ]
}

#[tokio::test]
async fn restart_resync_is_idempotent_and_never_overwrites_same_named_local_profile() {
    let path = temp_database("restart");
    remove_database(&path);
    let store = Arc::new(SqliteStore::open(&path).await.expect("store"));
    seed_local(store.as_ref()).await;

    let initial = store
        .begin_federation_sync(account(7), machine("max"))
        .await
        .expect("begin");
    assert_eq!(initial.generation, 0);
    let applied = store
        .apply_federation_page(account(7), machine("max"), None, None, remote_records(20))
        .await
        .expect("apply");
    assert!(applied.complete);
    assert_eq!(applied.records_applied, 3);
    drop(store);

    let store = SqliteStore::open(&path).await.expect("reopen");
    let resumed = store
        .begin_federation_sync(account(7), machine("max"))
        .await
        .expect("resync");
    assert_eq!(resumed.generation, 1);
    assert_eq!(resumed.pages_applied, 1);
    let replayed = store
        .apply_federation_page(account(7), machine("max"), None, None, remote_records(20))
        .await
        .expect("idempotent replay");
    assert_eq!(replayed.records_applied, 3);
    assert_eq!(replayed.pages_applied, 2);

    let local = store
        .get_profile(account(7), profile("claude"))
        .await
        .expect("profile read")
        .expect("profile");
    assert_eq!(local.origin, ProfileOrigin::Local);
    assert_eq!(
        local.config_dir,
        Some(PathBuf::from("/private/local-claude"))
    );
    assert_eq!(
        store
            .usage_history(account(7), profile("claude"), None, 10)
            .await
            .expect("usage")
            .len(),
        1
    );

    let state = store
        .begin_federation_sync(account(7), machine("max"))
        .await
        .expect("next generation");
    assert_eq!(state.generation, 2);
    let error = store
        .apply_federation_page(account(7), machine("max"), None, None, remote_records(30))
        .await
        .expect_err("stable key mutation must fail");
    assert_eq!(error.kind(), atmux::pulse::PulseErrorKind::Conflict);
    let max = store
        .list_machines(account(7))
        .await
        .expect("machines")
        .into_iter()
        .find(|row| row.name == machine("max"))
        .expect("max");
    assert_eq!(max.last_seen, instant(20));
    remove_database(&path);
}

#[tokio::test]
async fn cross_account_or_conflicting_profile_rolls_back_the_whole_page_and_cursor() {
    let path = temp_database("atomic");
    remove_database(&path);
    let store = SqliteStore::open(&path).await.expect("store");
    seed_local(&store).await;
    store
        .begin_federation_sync(account(7), machine("midnight"))
        .await
        .expect("begin");
    let mut records = remote_records(20);
    for record in &mut records {
        record.origin = origin("midnight");
        match &mut record.row {
            FederatedPulseRow::Machine(row) => row.name = machine("midnight"),
            FederatedPulseRow::Profile(row) => row.account_id = account(8),
            FederatedPulseRow::Usage(row) => row.machine = machine("midnight"),
            FederatedPulseRow::Context(_) | FederatedPulseRow::Token(_) => {}
        }
    }
    let cursor = OpaqueCursor::new("v2.Yy8wMDAwMDAwMDAwMDAwMDAwMDAwMQ").expect("cursor");
    assert!(
        store
            .apply_federation_page(account(7), machine("midnight"), None, Some(cursor), records,)
            .await
            .is_err()
    );
    assert!(
        store
            .list_machines(account(7))
            .await
            .expect("machines")
            .into_iter()
            .all(|row| row.name != machine("midnight"))
    );
    let state = store
        .begin_federation_sync(account(7), machine("midnight"))
        .await
        .expect("state");
    assert_eq!(state.cursor, None);
    assert_eq!(state.pages_applied, 0);

    let mut vendor_conflict = remote_records(20);
    if let FederatedPulseRow::Profile(row) = &mut vendor_conflict[1].row {
        row.vendor = Vendor::OpenaiCodex;
    }
    store
        .begin_federation_sync(account(7), machine("max"))
        .await
        .expect("begin max");
    assert!(
        store
            .apply_federation_page(account(7), machine("max"), None, None, vendor_conflict)
            .await
            .is_err()
    );
    assert!(
        store
            .list_machines(account(7))
            .await
            .expect("machines")
            .into_iter()
            .all(|row| row.name != machine("max"))
    );
    remove_database(&path);
}
