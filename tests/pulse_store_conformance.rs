#![cfg(feature = "pulse")]

use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
};

use atmux::pulse::{
    Account, AccountId, AgentSettings, AlertSubscription, AlertType, CollectionOutcome,
    ContextSession, Fraction, GeminiQuota, Instant, Machine, MachineName, Percent, Profile,
    ProfileName, QuotaWindow, QuotaWindowKind, RefreshPolicy, SessionId, TokenGrain, TokenSource,
    UsageSnapshot, Vendor,
    error::PulseErrorKind,
    federation::{FederatedPulseRow, FederatedRecord, PulseOrigin},
    ingest::{
        IngestTokenManager, MAX_ACTIVE_INGEST_TOKENS, PUSH_VERSION, PushBatch, PushEnvelope,
        REPORTER_VERSION,
    },
    store::{
        AlertEventInput, AlertReplyInput, ImportProvenance, IngestBatch, IngestLimits,
        IngestReplay, IngestToken, MAX_REPORTER_DESTINATIONS_PER_ACCOUNT, PricingRule,
        ReporterCursorState, ReporterPendingChunk, ReporterPendingDraft, ReporterStreamKind,
        ReporterTokenPosition, ResetResumeInput, ResetResumeLimits, SqliteStore, Store,
        TokenBackfillPage, schema::LATEST_SCHEMA_VERSION,
    },
    token::{TokenSourceGeneration, TokenTallyCursor},
};

#[cfg(feature = "pulse-postgres")]
use atmux::pulse::{
    PulseConfig, PulseDatabaseConfig, ops::open_doctor_store, store::PostgresStore,
};

#[cfg(feature = "pulse-postgres")]
static POSTGRES_TEST_LOCK: std::sync::OnceLock<Arc<tokio::sync::Mutex<()>>> =
    std::sync::OnceLock::new();

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

struct TestStore {
    path: Option<PathBuf>,
    store: Arc<dyn Store>,
    #[cfg(feature = "pulse-postgres")]
    postgres_url: Option<String>,
    #[cfg(feature = "pulse-postgres")]
    _postgres_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl TestStore {
    async fn new() -> Self {
        #[cfg(feature = "pulse-postgres")]
        if let Ok(url) = std::env::var("ATMUX_PULSE_TEST_POSTGRES_URL") {
            let lock = Arc::clone(
                POSTGRES_TEST_LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))),
            );
            let guard = lock.lock_owned().await;
            reset_postgres(&url).await;
            let store = PostgresStore::connect(&url)
                .await
                .expect("open PostgreSQL test store");
            return Self {
                path: None,
                store: Arc::new(store),
                postgres_url: Some(url),
                _postgres_guard: Some(guard),
            };
        }
        let id = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("atmux-pulse-store-{}-{id}", std::process::id()));
        std::fs::create_dir(&directory).expect("create private SQLite test directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .expect("secure SQLite test directory");
        }
        let path = directory.join("pulse.sqlite3");
        remove_sqlite_files(&path);
        let store = SqliteStore::open(&path).await.expect("open test store");
        Self {
            path: Some(path),
            store: Arc::new(store),
            #[cfg(feature = "pulse-postgres")]
            postgres_url: None,
            #[cfg(feature = "pulse-postgres")]
            _postgres_guard: None,
        }
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            remove_sqlite_files(path);
            if let Some(parent) = path.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
}

#[cfg(feature = "pulse-postgres")]
async fn reset_postgres(url: &str) {
    let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect disposable PostgreSQL database");
    let driver = tokio::spawn(connection);
    client
        .batch_execute("DROP SCHEMA IF EXISTS atmux_pulse CASCADE")
        .await
        .expect("reset disposable PostgreSQL schema");
    drop(client);
    driver
        .await
        .expect("join PostgreSQL reset driver")
        .expect("drive PostgreSQL reset");
}

fn remove_sqlite_files(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let _ = fs::remove_file(PathBuf::from(wal));
    let mut shm = path.as_os_str().to_owned();
    shm.push("-shm");
    let _ = fs::remove_file(PathBuf::from(shm));
}

fn account_id(value: i64) -> AccountId {
    AccountId::new(value).expect("account id")
}

fn profile_name(value: &str) -> ProfileName {
    ProfileName::new(value).expect("profile name")
}

fn machine_name(value: &str) -> MachineName {
    MachineName::new(value).expect("machine name")
}

fn instant(value: i64) -> Instant {
    Instant::from_epoch_millis(value).expect("instant")
}

fn assert_close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1.0e-9, "{actual} != {expected}");
}

fn profile(account: AccountId, name: &str, vendor: Vendor) -> Profile {
    Profile {
        account_id: account,
        name: profile_name(name),
        vendor,
        config_dir: Some(PathBuf::from(format!("/tmp/{name}"))),
        poll_interval_minutes: 15,
        monthly_budget_usd: (vendor == Vendor::DeepseekBalance).then_some(100.0),
        api_key_env: (vendor == Vendor::DeepseekBalance).then_some("DEEPSEEK_KEY".to_owned()),
        api_key_file: None,
        refresh: RefreshPolicy::InMemory,
        hidden: false,
        origin: atmux::pulse::ProfileOrigin::Local,
    }
}

async fn seed(store: &dyn Store) {
    for (id, identity) in [
        (account_id(1), "one@example.test"),
        (account_id(2), "two@example.test"),
    ] {
        store
            .upsert_account(Account {
                id,
                identity: identity.to_owned(),
                display_name: None,
            })
            .await
            .expect("seed account");
        for machine in ["midnight", "max"] {
            store
                .upsert_machine(Machine {
                    account_id: id,
                    name: machine_name(machine),
                    first_seen: instant(1_000),
                    last_seen: instant(2_000),
                })
                .await
                .expect("seed machine");
        }
        store
            .upsert_profile(profile(id, "claude", Vendor::AnthropicOauth))
            .await
            .expect("seed profile");
        store
            .upsert_profile(profile(id, "codex", Vendor::OpenaiCodex))
            .await
            .expect("seed codex");
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one backend-parameterized scenario must prove page atomicity, resync idempotence, and local-profile preservation together"
)]
async fn federation_consumer_is_atomic_idempotent_and_preserves_local_profile_metadata() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    let max = machine_name("max");
    let origin = PulseOrigin {
        machine: max.clone(),
        path: vec![max.clone()],
    };
    let mut reported = profile(account, "claude", Vendor::AnthropicOauth);
    reported.config_dir = None;
    reported.api_key_env = None;
    reported.api_key_file = None;
    reported.refresh = RefreshPolicy::Never;
    reported.origin = atmux::pulse::ProfileOrigin::Reported;
    let records = vec![
        FederatedRecord {
            key: "a/max".to_owned(),
            origin: origin.clone(),
            row: FederatedPulseRow::Machine(Machine {
                account_id: account,
                name: max.clone(),
                first_seen: instant(1_000),
                last_seen: instant(3_000),
            }),
        },
        FederatedRecord {
            key: "b/claude".to_owned(),
            origin: origin.clone(),
            row: FederatedPulseRow::Profile(reported),
        },
        FederatedRecord {
            key: "c/00000000000000000001".to_owned(),
            origin,
            row: FederatedPulseRow::Usage(snapshot(
                account,
                "claude",
                "max",
                Vendor::AnthropicOauth,
                10_000,
                vec![window(QuotaWindowKind::FiveHour, 10.0, 100_000)],
            )),
        },
    ];
    database
        .store
        .begin_federation_sync(account, max.clone())
        .await
        .expect("begin federation");
    let state = database
        .store
        .apply_federation_page(account, max.clone(), None, None, records.clone())
        .await
        .expect("apply federation");
    assert!(state.complete);
    assert_eq!(state.records_applied, 3);
    database
        .store
        .begin_federation_sync(account, max.clone())
        .await
        .expect("begin resync");
    let replay = database
        .store
        .apply_federation_page(account, max, None, None, records)
        .await
        .expect("idempotent replay");
    assert_eq!(replay.records_applied, 3);
    assert_eq!(
        database
            .store
            .usage_history(account, profile_name("claude"), None, 10)
            .await
            .expect("usage")
            .len(),
        1
    );
    let local = database
        .store
        .get_profile(account, profile_name("claude"))
        .await
        .expect("profile")
        .expect("local profile");
    assert_eq!(local.origin, atmux::pulse::ProfileOrigin::Local);
    assert_eq!(local.config_dir, Some(PathBuf::from("/tmp/claude")));
    let exported = database
        .store
        .local_federation_page(account, machine_name("max"), None, 501)
        .await
        .expect("SQL-bounded local export");
    assert!(exported.iter().all(|record| match &record.row {
        FederatedPulseRow::Machine(row) => row.name == machine_name("max"),
        FederatedPulseRow::Profile(row) => {
            row.origin == atmux::pulse::ProfileOrigin::Reported
                && row.config_dir.is_none()
                && row.api_key_env.is_none()
                && row.api_key_file.is_none()
        }
        FederatedPulseRow::Usage(row) => row.machine == machine_name("max"),
        FederatedPulseRow::Context(row) => row.machine == machine_name("max"),
        FederatedPulseRow::Token(row) => row.machine == machine_name("max"),
    }));

    let new_machine = machine_name("remote-new");
    database
        .store
        .begin_federation_sync(account, new_machine.clone())
        .await
        .expect("begin invalid page");
    let invalid_origin = PulseOrigin {
        machine: new_machine.clone(),
        path: vec![new_machine.clone()],
    };
    let invalid = vec![
        FederatedRecord {
            key: "a/remote-new".to_owned(),
            origin: invalid_origin.clone(),
            row: FederatedPulseRow::Machine(Machine {
                account_id: account,
                name: new_machine.clone(),
                first_seen: instant(1),
                last_seen: instant(2),
            }),
        },
        FederatedRecord {
            key: "b/cross-account".to_owned(),
            origin: invalid_origin,
            row: FederatedPulseRow::Profile({
                let mut row = profile(account_id(2), "claude", Vendor::AnthropicOauth);
                row.config_dir = None;
                row.refresh = RefreshPolicy::Never;
                row.origin = atmux::pulse::ProfileOrigin::Reported;
                row
            }),
        },
    ];
    assert!(
        database
            .store
            .apply_federation_page(account, new_machine.clone(), None, None, invalid)
            .await
            .is_err()
    );
    assert!(
        database
            .store
            .list_machines(account)
            .await
            .expect("machines")
            .into_iter()
            .all(|row| row.name != new_machine)
    );
}

async fn seed_reporter_rows(store: &dyn Store, account: AccountId) {
    for (machine, polled) in [("midnight", 10_000), ("max", 20_000)] {
        store
            .append_usage_snapshot(snapshot(
                account,
                "claude",
                machine,
                Vendor::AnthropicOauth,
                polled,
                vec![window(QuotaWindowKind::FiveHour, 10.0, 100_000)],
            ))
            .await
            .expect("seed reporter usage");
        store
            .upsert_token_grain(token(account, "claude", machine, machine, 10))
            .await
            .expect("seed reporter token");
    }
}

async fn advance_reporter_usage(
    store: &dyn Store,
    account: AccountId,
    destination: &str,
) -> ReporterCursorState {
    let initial = store
        .load_reporter_cursor(account, machine_name("midnight"), destination.to_owned())
        .await
        .expect("load reporter cursor");
    assert_eq!(initial.usage_after_id, 0);
    let usage = store
        .local_reporter_usage_page(account, machine_name("midnight"), 0, 500)
        .await
        .expect("local usage page");
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].snapshot.machine, machine_name("midnight"));
    let mut after_usage = initial.clone();
    after_usage.usage_after_id = usage[0].id;
    let stored = store
        .advance_reporter_cursor(
            account,
            machine_name("midnight"),
            destination.to_owned(),
            initial.clone(),
            after_usage,
        )
        .await
        .expect("advance usage cursor");
    assert!(
        store
            .local_reporter_usage_page(
                account,
                machine_name("midnight"),
                stored.usage_after_id,
                500,
            )
            .await
            .expect("usage resume")
            .is_empty()
    );
    assert_eq!(
        store
            .advance_reporter_cursor(
                account,
                machine_name("midnight"),
                destination.to_owned(),
                initial,
                stored.clone(),
            )
            .await
            .expect_err("stale cursor must fail")
            .kind(),
        PulseErrorKind::Conflict
    );
    stored
}

fn reporter_usage_pending_draft(
    account: AccountId,
    machine: MachineName,
    expected: ReporterCursorState,
    next: ReporterCursorState,
    usage: UsageSnapshot,
) -> ReporterPendingDraft {
    let request_id = "push-store-conformance".to_owned();
    let body = PushEnvelope {
        version: PUSH_VERSION,
        request_id: request_id.clone(),
        reporter_version: REPORTER_VERSION.to_owned(),
        account_id: Some(account),
        machine: Some(machine),
        batch: PushBatch {
            snapshots: vec![usage],
            ..PushBatch::default()
        },
    }
    .encode()
    .expect("encode reporter outbox page");
    ReporterPendingDraft {
        kind: ReporterStreamKind::Usage,
        expected,
        next,
        chunks: vec![ReporterPendingChunk {
            request_id,
            body,
            rows: 1,
        }],
    }
}

async fn roundtrip_reporter_outbox(
    store: &dyn Store,
    account: AccountId,
    destination: &str,
    expected: ReporterCursorState,
) -> ReporterCursorState {
    store
        .append_usage_snapshot(snapshot(
            account,
            "claude",
            "midnight",
            Vendor::AnthropicOauth,
            11_000,
            vec![window(QuotaWindowKind::FiveHour, 11.0, 101_000)],
        ))
        .await
        .expect("append outbox usage");
    let page = store
        .local_reporter_usage_page(
            account,
            machine_name("midnight"),
            expected.usage_after_id,
            500,
        )
        .await
        .expect("load outbox usage");
    let mut next = expected.clone();
    next.usage_after_id = page[0].id;
    let draft = reporter_usage_pending_draft(
        account,
        machine_name("midnight"),
        expected,
        next.clone(),
        page[0].snapshot.clone(),
    );
    let pending = store
        .prepare_reporter_pending(
            account,
            machine_name("midnight"),
            destination.to_owned(),
            draft,
        )
        .await
        .expect("prepare reporter outbox");
    assert_eq!(
        store
            .load_reporter_pending(
                account,
                machine_name("midnight"),
                destination.to_owned(),
                ReporterStreamKind::Usage,
            )
            .await
            .expect("reload reporter outbox"),
        Some(pending.clone())
    );
    let stored = store
        .commit_reporter_pending(
            account,
            machine_name("midnight"),
            destination.to_owned(),
            ReporterStreamKind::Usage,
            pending.id,
        )
        .await
        .expect("commit reporter outbox");
    assert_eq!(stored, next);
    stored
}

#[tokio::test]
async fn reporter_pages_are_local_keyset_and_cursor_cas_is_durable() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    seed_reporter_rows(database.store.as_ref(), account).await;
    let destination =
        "reporter-v1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned();
    let stored = advance_reporter_usage(database.store.as_ref(), account, &destination).await;
    let stored =
        roundtrip_reporter_outbox(database.store.as_ref(), account, &destination, stored).await;

    let tokens = database
        .store
        .local_reporter_token_page(account, machine_name("midnight"), None, 500)
        .await
        .expect("local token page");
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].machine, machine_name("midnight"));
    let mut after_token = stored.clone();
    after_token.token_after = Some(
        ReporterTokenPosition::from_grain(tokens.first().expect("token")).expect("token position"),
    );
    database
        .store
        .advance_reporter_cursor(
            account,
            machine_name("midnight"),
            destination.clone(),
            stored,
            after_token.clone(),
        )
        .await
        .expect("advance token cursor");
    assert!(
        database
            .store
            .local_reporter_token_page(
                account,
                machine_name("midnight"),
                after_token.token_after.clone(),
                500,
            )
            .await
            .expect("token resume")
            .is_empty()
    );
    assert_eq!(
        database
            .store
            .load_reporter_cursor(account, machine_name("midnight"), destination)
            .await
            .expect("reload durable cursor"),
        after_token
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one backend-parameterized lifecycle must prove cursor atomicity, stale-write rejection, scope, restart, and completion together"
)]
async fn token_backfill_pages_are_atomic_resumable_and_generation_scoped() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    let profile = profile_name("claude");
    let machine = machine_name("midnight");
    let generation_a = TokenSourceGeneration::new("a".repeat(64)).expect("generation A");
    let generation_b = TokenSourceGeneration::new("b".repeat(64)).expect("generation B");

    let initial = database
        .store
        .begin_token_backfill(
            account,
            profile.clone(),
            machine.clone(),
            generation_a.clone(),
            false,
        )
        .await
        .expect("begin backfill");
    assert_eq!(initial.generation, 1);
    assert!(initial.cursor.is_none());
    assert!(!initial.complete);

    let mut first = token(account, "claude", "midnight", "session-01", 10);
    first.source = TokenSource::Local;
    let first_cursor = TokenTallyCursor::from_grain(&first);
    let after_first = database
        .store
        .apply_token_backfill_page(TokenBackfillPage {
            expected: initial.clone(),
            rows: vec![first],
            next_cursor: Some(first_cursor.clone()),
            complete: false,
        })
        .await
        .expect("atomically apply first page");
    assert_eq!(after_first.cursor, Some(first_cursor));
    assert!(!after_first.complete);
    assert_eq!(
        database
            .store
            .begin_token_backfill(
                account,
                profile.clone(),
                machine.clone(),
                generation_a.clone(),
                false,
            )
            .await
            .expect("resume incomplete generation"),
        after_first
    );

    let mut stale_row = token(account, "claude", "midnight", "session-stale", 99);
    stale_row.source = TokenSource::Local;
    let stale_cursor = TokenTallyCursor::from_grain(&stale_row);
    assert_eq!(
        database
            .store
            .apply_token_backfill_page(TokenBackfillPage {
                expected: initial,
                rows: vec![stale_row],
                next_cursor: Some(stale_cursor),
                complete: false,
            })
            .await
            .expect_err("stale cursor must fail without writes")
            .kind(),
        PulseErrorKind::Conflict
    );
    assert_eq!(
        database
            .store
            .list_token_grains(account, Some(profile.clone()), None, 10)
            .await
            .expect("tokens after stale page")
            .len(),
        1
    );

    let reset = database
        .store
        .begin_token_backfill(
            account,
            profile.clone(),
            machine.clone(),
            generation_b.clone(),
            false,
        )
        .await
        .expect("restart changed source generation");
    assert_eq!(reset.generation, 2);
    assert!(reset.cursor.is_none());

    let mut cross_account = token(account_id(2), "claude", "midnight", "session-02", 20);
    cross_account.source = TokenSource::Local;
    let cross_cursor = TokenTallyCursor::from_grain(&cross_account);
    assert_eq!(
        database
            .store
            .apply_token_backfill_page(TokenBackfillPage {
                expected: reset.clone(),
                rows: vec![cross_account],
                next_cursor: Some(cross_cursor),
                complete: true,
            })
            .await
            .expect_err("cross-account row must fail closed")
            .kind(),
        PulseErrorKind::Conflict
    );

    let mut final_row = token(account, "claude", "midnight", "session-02", 20);
    final_row.source = TokenSource::Local;
    let final_cursor = TokenTallyCursor::from_grain(&final_row);
    let complete = database
        .store
        .apply_token_backfill_page(TokenBackfillPage {
            expected: reset,
            rows: vec![final_row],
            next_cursor: Some(final_cursor),
            complete: true,
        })
        .await
        .expect("complete changed-source generation");
    assert!(complete.complete);
    assert_eq!(
        database
            .store
            .list_token_grains(account, Some(profile.clone()), None, 10)
            .await
            .expect("completed token rows")
            .len(),
        2
    );

    let explicit_rerun = database
        .store
        .begin_token_backfill(account, profile, machine, generation_b, true)
        .await
        .expect("explicit rerun starts a new generation");
    assert_eq!(explicit_rerun.generation, 3);
    assert!(explicit_rerun.cursor.is_none());
    assert!(!explicit_rerun.complete);
}

async fn independent_reporter_stores(database: &TestStore) -> Vec<Arc<dyn Store>> {
    if let Some(path) = &database.path {
        let mut stores = Vec::new();
        for _ in 0..4 {
            let store = SqliteStore::open(path)
                .await
                .expect("open independent SQLite reporter store");
            stores.push(Arc::new(store) as Arc<dyn Store>);
        }
        return stores;
    }
    #[cfg(feature = "pulse-postgres")]
    if let Some(url) = &database.postgres_url {
        let mut stores = Vec::new();
        for _ in 0..4 {
            let store = PostgresStore::connect(url)
                .await
                .expect("open independent PostgreSQL reporter store");
            stores.push(Arc::new(store) as Arc<dyn Store>);
        }
        return stores;
    }
    panic!("test database backend is unavailable");
}

#[tokio::test]
async fn concurrent_reporter_destination_cap_is_atomic_per_account() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let stores = independent_reporter_stores(&database).await;
    let attempts = MAX_REPORTER_DESTINATIONS_PER_ACCOUNT + 12;
    let mut tasks = Vec::with_capacity(attempts);
    for index in 0..attempts {
        let store = Arc::clone(&stores[index % stores.len()]);
        tasks.push(tokio::spawn(async move {
            let destination = format!("reporter-v1-{index:064x}");
            store
                .load_reporter_cursor(account_id(1), machine_name("midnight"), destination)
                .await
        }));
    }
    let mut created = 0_usize;
    let mut conflicts = 0_usize;
    for task in tasks {
        match task.await.expect("join destination creator") {
            Ok(_) => created = created.saturating_add(1),
            Err(error) => {
                assert_eq!(error.kind(), PulseErrorKind::Conflict);
                conflicts = conflicts.saturating_add(1);
            }
        }
    }
    assert_eq!(created, MAX_REPORTER_DESTINATIONS_PER_ACCOUNT);
    assert_eq!(conflicts, 12);
    stores[0]
        .load_reporter_cursor(
            account_id(1),
            machine_name("midnight"),
            format!("reporter-v1-{:064x}", 0),
        )
        .await
        .expect("existing destination remains usable at cap");
    assert_eq!(
        stores[0]
            .load_reporter_cursor(
                account_id(1),
                machine_name("max"),
                "reporter-v1-account-cap-extra".to_owned(),
            )
            .await
            .expect_err("cap spans every machine in one account")
            .kind(),
        PulseErrorKind::Conflict
    );
}

#[tokio::test]
async fn concurrent_receiver_token_cap_is_atomic_and_failed_issuance_leaves_no_machine() {
    let database = TestStore::new().await;
    let account = account_id(31);
    database
        .store
        .upsert_account(Account {
            id: account,
            identity: "receiver@example.test".to_owned(),
            display_name: None,
        })
        .await
        .expect("seed receiver account");
    let manager = IngestTokenManager::new(Arc::clone(&database.store));
    let mut issuers = Vec::new();
    for index in 0..(MAX_ACTIVE_INGEST_TOKENS + 12) {
        let manager = manager.clone();
        issuers.push(tokio::spawn(async move {
            manager
                .issue(
                    account,
                    machine_name(&format!("reporter-{index}")),
                    instant(10_000 + i64::try_from(index).expect("index")),
                )
                .await
        }));
    }
    let mut issued = Vec::new();
    let mut conflicts = 0;
    for issuer in issuers {
        match issuer.await.expect("join issuer") {
            Ok(token) => issued.push(token),
            Err(error) => {
                assert_eq!(error.kind(), PulseErrorKind::Conflict);
                conflicts += 1;
            }
        }
    }
    assert_eq!(issued.len(), MAX_ACTIVE_INGEST_TOKENS);
    assert_eq!(conflicts, 12);
    assert_eq!(
        database
            .store
            .list_ingest_tokens(account)
            .await
            .expect("list tokens")
            .len(),
        MAX_ACTIVE_INGEST_TOKENS
    );
    assert_eq!(
        database
            .store
            .list_machines(account)
            .await
            .expect("list machines")
            .len(),
        MAX_ACTIVE_INGEST_TOKENS,
        "machines from rejected issuers must roll back with token issuance"
    );

    let first_id = issued[0].summary.id;
    assert!(
        manager
            .revoke(account, first_id, instant(20_000))
            .await
            .expect("revoke own token")
    );
    assert!(
        !manager
            .revoke(account_id(32), first_id, instant(20_000))
            .await
            .expect("cross-account revoke")
    );
    manager
        .issue(account, machine_name("replacement"), instant(21_000))
        .await
        .expect("revocation frees one active slot");
    assert_eq!(
        database
            .store
            .list_machines(account)
            .await
            .expect("list machines after replacement")
            .len(),
        MAX_ACTIVE_INGEST_TOKENS + 1
    );
}

fn window(kind: QuotaWindowKind, used: f64, reset: i64) -> QuotaWindow {
    QuotaWindow {
        kind,
        used_percent: Percent::new(used).expect("percentage"),
        resets_at: instant(reset),
    }
}

fn snapshot(
    account: AccountId,
    profile: &str,
    machine: &str,
    vendor: Vendor,
    polled: i64,
    windows: Vec<QuotaWindow>,
) -> UsageSnapshot {
    UsageSnapshot {
        account_id: account,
        profile: profile_name(profile),
        machine: machine_name(machine),
        vendor,
        windows,
        outcome: CollectionOutcome::Success,
        polled_at: instant(polled),
        reporter_version: Some(format!("test-{machine}")),
    }
}

fn settings() -> AgentSettings {
    AgentSettings {
        service_tier: Some("priority".to_owned()),
        effort: Some("high".to_owned()),
        additional: BTreeMap::new(),
    }
}

fn token(
    account: AccountId,
    profile: &str,
    machine: &str,
    session: &str,
    tokens: u64,
) -> TokenGrain {
    let settings = settings();
    TokenGrain {
        account_id: account,
        profile: profile_name(profile),
        machine: machine_name(machine),
        session_id: SessionId::new(session).expect("session"),
        model: "claude-opus-5".to_owned(),
        settings_hash: settings.sha256().expect("settings hash"),
        settings,
        day: "2026-08-08".to_owned(),
        tokens_in: tokens,
        tokens_out: 2,
        cache_write_5m: 3,
        cache_write_1h: 4,
        cache_read: 5,
        source: TokenSource::Ingest,
    }
}

fn context(
    account: AccountId,
    profile: &str,
    machine: &str,
    session: &str,
    tokens: u64,
    collected: i64,
) -> ContextSession {
    ContextSession {
        account_id: account,
        profile: profile_name(profile),
        machine: machine_name(machine),
        session_id: SessionId::new(session).expect("session"),
        model: Some("claude-opus-5".to_owned()),
        settings: settings(),
        context_tokens: Some(tokens),
        context_percent: Some(Percent::new(50.0).expect("percent")),
        effective_limit: Some(200_000),
        last_active_at: instant(collected),
        last_reset_at: None,
        collected_at: instant(collected),
    }
}

#[tokio::test]
async fn migrations_pragmas_integrity_and_replay_are_sound() {
    let database = TestStore::new().await;
    assert_eq!(
        database.store.schema_version().await.expect("version"),
        LATEST_SCHEMA_VERSION
    );
    assert_eq!(
        database.store.integrity_check().await.expect("integrity"),
        "ok"
    );
    if let Some(path) = &database.path {
        let sqlite = SqliteStore::open(path).await.expect("reopen SQLite DB");
        let pragmas = sqlite.pragmas().await.expect("pragmas");
        assert_eq!(pragmas.journal_mode.to_ascii_lowercase(), "wal");
        assert!(pragmas.foreign_keys);
        assert_eq!(pragmas.busy_timeout_ms, 15_000);
        assert_eq!(
            sqlite.schema_version().await.expect("replayed version"),
            LATEST_SCHEMA_VERSION
        );
        assert_eq!(sqlite.integrity_check().await.expect("integrity"), "ok");
    }
    #[cfg(feature = "pulse-postgres")]
    if let Some(url) = &database.postgres_url {
        let reopened = PostgresStore::connect(url)
            .await
            .expect("reopen migrated PostgreSQL DB");
        assert_eq!(
            reopened.schema_version().await.expect("replayed version"),
            LATEST_SCHEMA_VERSION
        );
        assert_eq!(reopened.integrity_check().await.expect("integrity"), "ok");
    }
}

#[tokio::test]
async fn populated_v4_duplicate_provenance_upgrades_deterministically() {
    assert_sqlite_v4_duplicate_provenance_upgrade().await;
    #[cfg(feature = "pulse-postgres")]
    if let Ok(url) = std::env::var("ATMUX_PULSE_TEST_POSTGRES_URL") {
        assert_postgres_v4_duplicate_provenance_upgrade(&url).await;
    }
}

#[cfg(feature = "pulse-postgres")]
#[tokio::test]
async fn postgres_doctor_is_non_migrating_read_only_and_preserves_all_table_counts() {
    let Ok(url) = std::env::var("ATMUX_PULSE_TEST_POSTGRES_URL") else {
        return;
    };
    let lock = Arc::clone(POSTGRES_TEST_LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))));
    let _guard = lock.lock_owned().await;
    reset_postgres(&url).await;
    let store = PostgresStore::connect(&url)
        .await
        .expect("migrate PostgreSQL before doctor");
    let account = Account {
        id: account_id(41),
        identity: "doctor@example.test".to_owned(),
        display_name: None,
    };
    store
        .upsert_account(account.clone())
        .await
        .expect("seed doctor account");
    drop(store);

    let before = postgres_schema_and_counts(&url).await;
    let config = PulseConfig {
        database: PulseDatabaseConfig {
            sqlite_path: None,
            postgres_url_env: Some("ATMUX_PULSE_TEST_POSTGRES_URL".to_owned()),
        },
        ..PulseConfig::default()
    };
    let doctor = open_doctor_store(&config)
        .await
        .expect("open read-only PostgreSQL doctor store");
    assert_eq!(
        doctor.schema_version().await.expect("doctor schema"),
        LATEST_SCHEMA_VERSION
    );
    assert_eq!(doctor.integrity_check().await.expect("doctor health"), "ok");
    assert!(doctor.upsert_account(account).await.is_err());
    drop(doctor);
    let after = postgres_schema_and_counts(&url).await;
    assert_eq!(after, before);
    reset_postgres(&url).await;
}

#[cfg(feature = "pulse-postgres")]
async fn postgres_schema_and_counts(
    url: &str,
) -> (Vec<(String, String, String, String)>, Vec<(String, i64)>) {
    let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect PostgreSQL inspector");
    let driver = tokio::spawn(connection);
    let role = client
        .query_one(
            "SELECT rolsuper,rolbypassrls FROM pg_catalog.pg_roles WHERE rolname=current_user",
            &[],
        )
        .await
        .expect("inspect PostgreSQL role");
    assert!(
        !role.get::<_, bool>(0),
        "doctor test role must not be superuser"
    );
    assert!(
        !role.get::<_, bool>(1),
        "doctor test role must not bypass RLS"
    );
    let columns = client
        .query(
            "SELECT table_name,column_name,data_type,is_nullable \
             FROM information_schema.columns WHERE table_schema='atmux_pulse' \
             ORDER BY table_name,ordinal_position",
            &[],
        )
        .await
        .expect("snapshot PostgreSQL schema")
        .into_iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect::<Vec<_>>();
    let tables = client
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema='atmux_pulse' AND table_type='BASE TABLE' ORDER BY table_name",
            &[],
        )
        .await
        .expect("list PostgreSQL tables");
    let mut counts = Vec::with_capacity(tables.len());
    for row in tables {
        let name = row.get::<_, String>(0);
        let quoted = name.replace('"', "\"\"");
        let count = client
            .query_one(
                &format!("SELECT COUNT(*) FROM atmux_pulse.\"{quoted}\""),
                &[],
            )
            .await
            .expect("count PostgreSQL table")
            .get::<_, i64>(0);
        counts.push((name, count));
    }
    drop(client);
    driver
        .await
        .expect("join PostgreSQL inspector")
        .expect("drive PostgreSQL inspector");
    (columns, counts)
}

const SQLITE_V4_TOKEN_USAGE_FIXTURE: &str = "\
CREATE TABLE token_usage (\
    account_id INTEGER NOT NULL, profile TEXT NOT NULL, machine TEXT NOT NULL,\
    session_id TEXT NOT NULL, model TEXT NOT NULL, settings_hash TEXT NOT NULL,\
    day TEXT NOT NULL, source_json TEXT NOT NULL,\
    PRIMARY KEY(account_id,profile,machine,session_id,model,settings_hash,day,source_json)\
) STRICT;";

#[cfg(feature = "pulse-postgres")]
const POSTGRES_V4_TOKEN_USAGE_FIXTURE: &str = "\
CREATE TABLE atmux_pulse.token_usage (\
    account_id BIGINT NOT NULL, profile TEXT NOT NULL, machine TEXT NOT NULL,\
    session_id TEXT NOT NULL, model TEXT NOT NULL, settings_hash TEXT NOT NULL,\
    day DATE NOT NULL, source JSONB NOT NULL,\
    PRIMARY KEY(account_id,profile,machine,session_id,model,settings_hash,day,source)\
);";

async fn assert_sqlite_v4_duplicate_provenance_upgrade() {
    let id = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "atmux-pulse-v4-upgrade-{}-{id}",
        std::process::id()
    ));
    std::fs::create_dir(&directory).expect("create v4 SQLite directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .expect("secure v4 SQLite directory");
    }
    let path = directory.join("pulse.sqlite3");
    {
        let connection = rusqlite::Connection::open(&path).expect("open v4 SQLite database");
        connection
            .execute_batch(
                "CREATE TABLE pulse_schema_migrations (\
                     version INTEGER PRIMARY KEY CHECK (version > 0),\
                     applied_at_ms INTEGER NOT NULL\
                 ) STRICT;\
                 INSERT INTO pulse_schema_migrations VALUES (4, 0);\
                 CREATE TABLE accounts (\
                     id INTEGER PRIMARY KEY CHECK (id > 0),\
                     identity TEXT NOT NULL UNIQUE,\
                     display_name TEXT\
                 ) STRICT;\
                 INSERT INTO accounts VALUES (1, 'one@example.com', NULL);\
                 CREATE TABLE machines (\
                     account_id INTEGER NOT NULL REFERENCES accounts(id),\
                     name TEXT NOT NULL,\
                     PRIMARY KEY(account_id,name)\
                 ) STRICT;\
                 CREATE TABLE profiles (\
                     account_id INTEGER NOT NULL REFERENCES accounts(id),\
                     name TEXT NOT NULL,\
                     PRIMARY KEY(account_id,name)\
                 ) STRICT;\
                 CREATE TABLE import_provenance (\
                     account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,\
                     source_fingerprint TEXT NOT NULL,\
                     source_table TEXT NOT NULL,\
                     source_row_id TEXT NOT NULL,\
                     target_key TEXT NOT NULL,\
                     imported_at_ms INTEGER NOT NULL,\
                     PRIMARY KEY (account_id, source_fingerprint, source_table, source_row_id)\
                 ) STRICT;",
            )
            .expect("create populated v4 SQLite schema");
        connection
            .execute_batch(SQLITE_V4_TOKEN_USAGE_FIXTURE)
            .expect("create v4 token usage table");
        for (fingerprint, row_id) in [("b".repeat(64), "2"), ("a".repeat(64), "1")] {
            connection
                .execute(
                    "INSERT INTO import_provenance VALUES (?1,?2,?3,?4,?5,?6)",
                    rusqlite::params![1_i64, fingerprint, "token_usage", row_id, "logical", 1_i64],
                )
                .expect("insert duplicate logical v4 provenance");
        }
    }
    let store = SqliteStore::open(&path)
        .await
        .expect("upgrade populated v4 SQLite");
    assert_eq!(
        store.schema_version().await.expect("SQLite version"),
        LATEST_SCHEMA_VERSION
    );
    drop(store);
    let connection = rusqlite::Connection::open(&path).expect("inspect upgraded SQLite");
    let retained = connection
        .query_row(
            "SELECT source_fingerprint,payload_fingerprint FROM import_provenance \
             WHERE account_id=1 AND source_table='token_usage' AND target_key='logical'",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("retained SQLite provenance");
    assert_eq!(retained, ("a".repeat(64), "0".repeat(64)));
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM import_provenance", [], |row| row
                .get::<_, i64>(0))
            .expect("SQLite provenance count"),
        1
    );
    drop(connection);
    remove_sqlite_files(&path);
    std::fs::remove_dir(&directory).expect("remove v4 SQLite directory");
}

#[cfg(feature = "pulse-postgres")]
async fn assert_postgres_v4_duplicate_provenance_upgrade(url: &str) {
    let lock = Arc::clone(POSTGRES_TEST_LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))));
    let _guard = lock.lock_owned().await;
    reset_postgres(url).await;
    let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect v4 PostgreSQL database");
    let driver = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE SCHEMA atmux_pulse;\
                 CREATE TABLE atmux_pulse.pulse_schema_migrations (\
                     version INTEGER PRIMARY KEY CHECK (version > 0),\
                     applied_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()\
                 );\
                 INSERT INTO atmux_pulse.pulse_schema_migrations(version) VALUES (4);\
                 CREATE TABLE atmux_pulse.accounts (\
                     id BIGINT PRIMARY KEY, identity TEXT NOT NULL UNIQUE, display_name TEXT\
                 );\
                 INSERT INTO atmux_pulse.accounts VALUES (1, 'one@example.com', NULL);\
                 CREATE TABLE atmux_pulse.machines (\
                     account_id BIGINT NOT NULL REFERENCES atmux_pulse.accounts(id),\
                     name TEXT NOT NULL, PRIMARY KEY(account_id,name)\
                 );\
                 CREATE TABLE atmux_pulse.profiles (\
                     account_id BIGINT NOT NULL REFERENCES atmux_pulse.accounts(id),\
                     name TEXT NOT NULL, PRIMARY KEY(account_id,name)\
                 );\
                 CREATE TABLE atmux_pulse.import_provenance (\
                     account_id BIGINT NOT NULL REFERENCES atmux_pulse.accounts(id),\
                     source_fingerprint TEXT NOT NULL, source_table TEXT NOT NULL,\
                     source_row_id TEXT NOT NULL, target_key TEXT NOT NULL,\
                     imported_at TIMESTAMPTZ NOT NULL,\
                     PRIMARY KEY (account_id, source_fingerprint, source_table, source_row_id)\
                 );\
                 INSERT INTO atmux_pulse.import_provenance VALUES\
                     (1, repeat('b',64), 'token_usage', '2', 'logical', clock_timestamp()),\
                     (1, repeat('a',64), 'token_usage', '1', 'logical', clock_timestamp());\
                 ALTER TABLE atmux_pulse.import_provenance ENABLE ROW LEVEL SECURITY;\
                 ALTER TABLE atmux_pulse.import_provenance FORCE ROW LEVEL SECURITY;\
                 CREATE POLICY account_scope ON atmux_pulse.import_provenance FOR ALL \
                 USING (COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE)) \
                 WITH CHECK (COALESCE(current_setting('atmux.pulse_bypass', true) = 'on', FALSE));",
        )
        .await
        .expect("create populated v4 PostgreSQL schema");
    client
        .batch_execute(POSTGRES_V4_TOKEN_USAGE_FIXTURE)
        .await
        .expect("create v4 PostgreSQL token usage table");
    drop(client);
    driver
        .await
        .expect("join v4 PostgreSQL driver")
        .expect("drive v4 PostgreSQL connection");

    let store = PostgresStore::connect(url)
        .await
        .expect("upgrade populated v4 PostgreSQL");
    assert_eq!(
        store.schema_version().await.expect("PostgreSQL version"),
        LATEST_SCHEMA_VERSION
    );
    drop(store);
    let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("inspect upgraded PostgreSQL");
    let driver = tokio::spawn(connection);
    client
        .batch_execute("SET atmux.pulse_bypass = 'on'")
        .await
        .expect("enable PostgreSQL inspection scope");
    let row = client
        .query_one(
            "SELECT source_fingerprint,payload_fingerprint \
                 FROM atmux_pulse.import_provenance",
            &[],
        )
        .await
        .expect("retained PostgreSQL provenance");
    assert_eq!(row.get::<_, String>(0), "a".repeat(64));
    assert_eq!(row.get::<_, String>(1), "0".repeat(64));
    assert_eq!(
        client
            .query_one("SELECT COUNT(*) FROM atmux_pulse.import_provenance", &[])
            .await
            .expect("PostgreSQL provenance count")
            .get::<_, i64>(0),
        1
    );
    assert_postgres_v7_token_usage_migrated(&client).await;
    drop(client);
    driver
        .await
        .expect("join PostgreSQL inspection driver")
        .expect("drive PostgreSQL inspection connection");
    reset_postgres(url).await;
}

#[cfg(feature = "pulse-postgres")]
async fn assert_postgres_v7_token_usage_migrated(client: &tokio_postgres::Client) {
    let columns = client
        .query_one(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema='atmux_pulse' AND table_name='token_usage' \
               AND column_name='write_revision'",
            &[],
        )
        .await
        .expect("inspect migrated PostgreSQL token usage table")
        .get::<_, i64>(0);
    assert_eq!(
        columns, 1,
        "the v4 fixture must continue through the token_usage v7 migration"
    );
}

#[tokio::test]
async fn a_newer_schema_fails_forward_only_without_mutation() {
    #[cfg(feature = "pulse-postgres")]
    if let Ok(url) = std::env::var("ATMUX_PULSE_TEST_POSTGRES_URL") {
        let lock =
            Arc::clone(POSTGRES_TEST_LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))));
        let _guard = lock.lock_owned().await;
        reset_postgres(&url).await;
        let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .expect("connect disposable PostgreSQL database");
        let driver = tokio::spawn(connection);
        client
            .batch_execute(
                "CREATE SCHEMA atmux_pulse; \
                 CREATE TABLE atmux_pulse.pulse_schema_migrations (\
                   version INTEGER PRIMARY KEY, applied_at TIMESTAMPTZ NOT NULL\
                 ); \
                 INSERT INTO atmux_pulse.pulse_schema_migrations \
                   VALUES (999, clock_timestamp());",
            )
            .await
            .expect("write newer PostgreSQL marker");
        drop(client);
        driver
            .await
            .expect("join PostgreSQL marker driver")
            .expect("drive PostgreSQL marker connection");
        let Err(error) = PostgresStore::connect(&url).await else {
            panic!("newer PostgreSQL schema must be rejected");
        };
        assert_eq!(error.kind(), PulseErrorKind::Configuration);
        let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .expect("inspect PostgreSQL marker");
        let driver = tokio::spawn(connection);
        assert_eq!(
            client
                .query_one(
                    "SELECT MAX(version) FROM atmux_pulse.pulse_schema_migrations",
                    &[],
                )
                .await
                .expect("read PostgreSQL marker")
                .get::<_, i32>(0),
            999
        );
        drop(client);
        driver
            .await
            .expect("join PostgreSQL inspection driver")
            .expect("drive PostgreSQL inspection connection");
        return;
    }
    let id = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "atmux-pulse-newer-{}-{id}.sqlite3",
        std::process::id()
    ));
    remove_sqlite_files(&path);
    {
        let connection = rusqlite::Connection::open(&path).expect("raw database");
        connection
            .execute_batch(
                "CREATE TABLE pulse_schema_migrations (version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL) STRICT; \
                 INSERT INTO pulse_schema_migrations VALUES (999, 0);",
            )
            .expect("newer marker");
    }
    let Err(error) = SqliteStore::open(&path).await else {
        panic!("newer schema must be rejected");
    };
    assert_eq!(error.kind(), PulseErrorKind::Configuration);
    let connection = rusqlite::Connection::open(&path).expect("inspect database");
    assert_eq!(
        connection
            .query_row(
                "SELECT MAX(version) FROM pulse_schema_migrations",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("version"),
        999
    );
    remove_sqlite_files(&path);
}

#[tokio::test]
async fn every_profile_mutation_is_account_scoped() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let one = account_id(1);
    let two = account_id(2);

    assert!(
        database
            .store
            .set_profile_hidden(one, profile_name("claude"), true)
            .await
            .expect("hide")
    );
    assert!(
        database
            .store
            .get_profile(one, profile_name("claude"))
            .await
            .expect("read")
            .expect("profile")
            .hidden
    );
    assert!(
        !database
            .store
            .get_profile(two, profile_name("claude"))
            .await
            .expect("read")
            .expect("profile")
            .hidden
    );
    assert!(
        database
            .store
            .delete_profile(one, profile_name("claude"))
            .await
            .expect("delete")
    );
    assert!(
        database
            .store
            .get_profile(one, profile_name("claude"))
            .await
            .expect("read")
            .is_none()
    );
    assert!(
        database
            .store
            .get_profile(two, profile_name("claude"))
            .await
            .expect("read")
            .is_some()
    );
}

#[cfg(feature = "pulse-postgres")]
async fn assert_cross_account_rls_insert_blocked(
    client: &mut tokio_postgres::Client,
    statement: &str,
    label: &str,
) {
    let transaction = client.transaction().await.expect("begin RLS write probe");
    transaction
        .query_one(
            "SELECT set_config('atmux.account_id', '1', true), \
                    set_config('atmux.pulse_bypass', 'off', true)",
            &[],
        )
        .await
        .expect("set RLS write probe scope");
    let error = transaction.execute(statement, &[]).await.expect_err(label);
    assert_eq!(
        error.as_db_error().expect("RLS database error").code(),
        &tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE
    );
    transaction
        .rollback()
        .await
        .expect("rollback RLS write probe");
}

#[cfg(feature = "pulse-postgres")]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn postgres_rls_is_forced_permissive_fail_closed_and_transaction_local() {
    if std::env::var("ATMUX_PULSE_TEST_POSTGRES_URL").is_err() {
        return;
    }
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    for (account, input) in [(account_id(1), 1.0), (account_id(2), 2.0)] {
        database
            .store
            .upsert_pricing_override(account, pricing("rls-delete", input))
            .await
            .expect("seed pricing RLS probe");
    }
    database
        .store
        .begin_token_observation(
            account_id(2),
            profile_name("claude"),
            machine_name("midnight"),
        )
        .await
        .expect("seed token revision RLS probe");
    database
        .store
        .begin_token_backfill(
            account_id(2),
            profile_name("claude"),
            machine_name("midnight"),
            TokenSourceGeneration::new("b".repeat(64)).expect("generation"),
            false,
        )
        .await
        .expect("seed backfill RLS probe");
    let url = database.postgres_url.as_deref().expect("PostgreSQL URL");
    let (mut client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect RLS probe");
    let driver = tokio::spawn(connection);

    let role = client
        .query_one(
            "SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
            &[],
        )
        .await
        .expect("inspect runtime role");
    assert!(!role.get::<_, bool>(0), "RLS probe must not use SUPERUSER");
    assert!(!role.get::<_, bool>(1), "RLS probe must not use BYPASSRLS");

    assert_eq!(
        client
            .query_one("SELECT COUNT(*) FROM atmux_pulse.profiles", &[])
            .await
            .expect("unset account is fail closed")
            .get::<_, i64>(0),
        0
    );
    let transaction = client.transaction().await.expect("begin account probe");
    transaction
        .query_one(
            "SELECT set_config('atmux.account_id', '1', true), \
                    set_config('atmux.pulse_bypass', 'off', true)",
            &[],
        )
        .await
        .expect("set account scope");
    assert_eq!(
        transaction
            .query_one("SELECT COUNT(*) FROM atmux_pulse.profiles", &[])
            .await
            .expect("read own account")
            .get::<_, i64>(0),
        2
    );
    assert_eq!(
        transaction
            .query_one(
                "SELECT COUNT(*) FROM atmux_pulse.profiles WHERE account_id = 2",
                &[],
            )
            .await
            .expect("other account is invisible")
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        transaction
            .execute(
                "UPDATE atmux_pulse.token_write_revisions SET revision=99 WHERE account_id=2",
                &[],
            )
            .await
            .expect("cross-account token revision update is a miss"),
        0
    );
    assert_eq!(
        transaction
            .execute(
                "UPDATE atmux_pulse.backfill_progress SET complete=TRUE WHERE account_id=2",
                &[],
            )
            .await
            .expect("cross-account backfill update is a miss"),
        0
    );
    assert_eq!(
        transaction
            .execute(
                "DELETE FROM atmux_pulse.pricing_overrides \
                 WHERE account_id = 2 AND key = 'rls-delete'",
                &[],
            )
            .await
            .expect("cross-account delete is indistinguishable from a miss"),
        0
    );
    transaction.commit().await.expect("commit account probe");
    assert_eq!(
        database
            .store
            .list_pricing_overrides(account_id(2))
            .await
            .expect("cross-account override survives RLS probe")
            .len(),
        1
    );

    let wrong_account = client
        .transaction()
        .await
        .expect("begin cross-account probe");
    wrong_account
        .query_one(
            "SELECT set_config('atmux.account_id', '1', true), \
                    set_config('atmux.pulse_bypass', 'off', true)",
            &[],
        )
        .await
        .expect("set cross-account probe scope");
    let error = wrong_account
        .execute(
            "INSERT INTO atmux_pulse.profiles \
             (account_id,name,vendor,poll_interval_minutes,refresh,hidden) \
             VALUES (2,'forbidden','\"anthropic-oauth\"'::jsonb,15,'\"in-memory\"'::jsonb,FALSE)",
            &[],
        )
        .await
        .expect_err("cross-account write must be blocked by RLS");
    assert_eq!(
        error.as_db_error().expect("RLS database error").code(),
        &tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE
    );
    wrong_account
        .rollback()
        .await
        .expect("rollback cross-account probe");

    let federation_scope = client
        .transaction()
        .await
        .expect("begin federation RLS probe");
    federation_scope
        .query_one(
            "SELECT set_config('atmux.account_id', '1', true), \
                    set_config('atmux.pulse_bypass', 'off', true)",
            &[],
        )
        .await
        .expect("set federation account scope");
    let error = federation_scope
        .execute(
            "INSERT INTO atmux_pulse.federation_peers (account_id, source_machine) \
             VALUES (2, 'forbidden-peer')",
            &[],
        )
        .await
        .expect_err("cross-account federation state must be blocked by RLS");
    assert_eq!(
        error.as_db_error().expect("RLS database error").code(),
        &tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE
    );
    federation_scope
        .rollback()
        .await
        .expect("rollback federation RLS probe");

    let reporter_scope = client
        .transaction()
        .await
        .expect("begin reporter RLS probe");
    reporter_scope
        .query_one(
            "SELECT set_config('atmux.account_id', '1', true), \
                    set_config('atmux.pulse_bypass', 'off', true)",
            &[],
        )
        .await
        .expect("set reporter account scope");
    let error = reporter_scope
        .execute(
            "INSERT INTO atmux_pulse.reporter_cursors \
             (account_id,machine,destination_key) VALUES (2,'midnight','forbidden')",
            &[],
        )
        .await
        .expect_err("cross-account reporter cursor must be blocked by RLS");
    assert_eq!(
        error.as_db_error().expect("RLS database error").code(),
        &tokio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE
    );
    reporter_scope
        .rollback()
        .await
        .expect("rollback reporter RLS probe");
    assert_cross_account_rls_insert_blocked(
        &mut client,
        "INSERT INTO atmux_pulse.reporter_pending_pages \
         (account_id,machine,destination_key,kind,expected_cursor,next_cursor,chunk_count,total_bytes) \
         VALUES (2,'midnight','forbidden','usage', \
         '{\"usage_after_id\":0,\"token_after\":null,\"token_generation\":0}'::jsonb, \
         '{\"usage_after_id\":1,\"token_after\":null,\"token_generation\":0}'::jsonb,1,2)",
        "cross-account reporter pending page must be blocked by RLS",
    )
    .await;
    assert_cross_account_rls_insert_blocked(
        &mut client,
        "INSERT INTO atmux_pulse.token_write_revisions \
         (account_id,profile,machine,revision) VALUES (2,'claude','max',1)",
        "cross-account token revision must be blocked by RLS",
    )
    .await;
    assert_cross_account_rls_insert_blocked(
        &mut client,
        "INSERT INTO atmux_pulse.backfill_progress \
         (account_id,profile,machine,generation,source_generation,write_revision,complete) \
         VALUES (2,'claude','max',1,repeat('a',64),1,FALSE)",
        "cross-account backfill progress must be blocked by RLS",
    )
    .await;
    assert_cross_account_rls_insert_blocked(
        &mut client,
        "INSERT INTO atmux_pulse.reporter_pending_chunks \
         (pending_id,account_id,chunk_index,request_id,body,rows) \
         VALUES (1,2,0,'forbidden',decode('7b7d','hex'),1)",
        "cross-account reporter pending chunk must be blocked by RLS",
    )
    .await;

    let bypass = client.transaction().await.expect("begin bypass probe");
    bypass
        .query_one(
            "SELECT set_config('atmux.account_id', '', true), \
                    set_config('atmux.pulse_bypass', 'on', true)",
            &[],
        )
        .await
        .expect("set transaction-local bypass");
    assert_eq!(
        bypass
            .query_one("SELECT COUNT(*) FROM atmux_pulse.profiles", &[])
            .await
            .expect("scoped bypass sees all accounts")
            .get::<_, i64>(0),
        4
    );
    bypass.commit().await.expect("commit bypass probe");
    assert_eq!(
        client
            .query_one("SELECT COUNT(*) FROM atmux_pulse.profiles", &[])
            .await
            .expect("bypass is restored")
            .get::<_, i64>(0),
        0
    );

    let policy = client
        .query_one(
            "SELECT COUNT(*), bool_and(c.relrowsecurity), bool_and(c.relforcerowsecurity), \
                    bool_and(p.permissive = 'PERMISSIVE'), bool_and(p.cmd = 'ALL') \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_policies p ON p.schemaname = n.nspname AND p.tablename = c.relname \
             WHERE n.nspname = 'atmux_pulse' AND c.relname = ANY($1)",
            &[&vec![
                "accounts",
                "machines",
                "profiles",
                "usage_snapshots",
                "usage_windows",
                "context_sessions",
                "token_usage",
                "pricing_overrides",
                "alert_subscriptions",
                "alert_events",
                "ingest_tokens",
                "gemini_quota",
                "import_provenance",
                "alert_replies",
                "reset_resume_jobs",
                "ingest_replays",
                "federation_peers",
                "federation_records",
                "reporter_cursors",
                "reporter_pending_pages",
                "reporter_pending_chunks",
                "backfill_progress",
                "token_write_revisions",
            ]],
        )
        .await
        .expect("inspect Pulse RLS policies");
    assert_eq!(policy.get::<_, i64>(0), 23);
    assert!(policy.get::<_, bool>(1));
    assert!(policy.get::<_, bool>(2));
    assert!(policy.get::<_, bool>(3));
    assert!(policy.get::<_, bool>(4));

    drop(client);
    driver
        .await
        .expect("join RLS probe driver")
        .expect("drive RLS probe connection");
}

#[cfg(feature = "pulse-postgres")]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn postgres_concurrent_caps_and_alert_cooldowns_are_atomic() {
    if std::env::var("ATMUX_PULSE_TEST_POSTGRES_URL").is_err() {
        return;
    }
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let url = database.postgres_url.as_deref().expect("PostgreSQL URL");
    let left = PostgresStore::connect(url).await.expect("left store");
    let right = PostgresStore::connect(url).await.expect("right store");
    let account = account_id(1);
    let machine = machine_name("midnight");
    let limits = IngestLimits {
        max_rows_per_request: 10,
        max_profiles: 4,
        max_usage_snapshots: 1,
        max_token_rows: 10,
        max_context_sessions: 10,
        max_gemini_models: 10,
    };
    let batch = |polled| IngestBatch {
        snapshots: vec![snapshot(
            account,
            "claude",
            "midnight",
            Vendor::AnthropicOauth,
            polled,
            vec![window(QuotaWindowKind::FiveHour, 10.0, 100_000)],
        )],
        ..IngestBatch::default()
    };
    let (first, second) = tokio::join!(
        left.ingest_batch(account, machine.clone(), batch(10_000), limits),
        right.ingest_batch(account, machine, batch(20_000), limits),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let rejected = if let Err(error) = first {
        error
    } else {
        second.expect_err("one concurrent ingest must be rejected")
    };
    assert_eq!(rejected.kind(), PulseErrorKind::Conflict);
    assert_eq!(
        database
            .store
            .usage_history(account, profile_name("claude"), None, 10)
            .await
            .expect("atomic snapshot history")
            .len(),
        1
    );

    let subscription = database
        .store
        .create_alert_subscription(
            AlertSubscription {
                account_id: account,
                profile: profile_name("claude"),
                alert_type: AlertType::AuthenticationFailure,
                threshold: None,
                cooldown_minutes: 30,
                delivery: None,
                enabled: true,
            },
            instant(1_000),
        )
        .await
        .expect("subscription");
    let event = AlertEventInput {
        account_id: account,
        subscription_id: subscription.id,
        profile: profile_name("claude"),
        alert_type: AlertType::AuthenticationFailure,
        message: "Authentication needs attention".to_owned(),
        current_value: None,
        threshold: None,
        triggered_at: instant(30_000),
    };
    let (first, second) = tokio::join!(
        left.record_alert_if_due(event.clone()),
        right.record_alert_if_due(event),
    );
    let inserted = usize::from(first.expect("first alert").is_some())
        + usize::from(second.expect("second alert").is_some());
    assert_eq!(inserted, 1);
}

#[tokio::test]
async fn account_and_machine_upserts_are_idempotent_and_monotonic() {
    let database = TestStore::new().await;
    let account = account_id(1);
    database
        .store
        .upsert_account(Account {
            id: account,
            identity: "first@example.test".to_owned(),
            display_name: None,
        })
        .await
        .expect("account");
    database
        .store
        .upsert_account(Account {
            id: account,
            identity: "renamed@example.test".to_owned(),
            display_name: Some("Ryan".to_owned()),
        })
        .await
        .expect("account update");
    assert_eq!(
        database
            .store
            .get_account(account)
            .await
            .expect("get")
            .expect("account")
            .identity,
        "renamed@example.test"
    );
    database
        .store
        .upsert_machine(Machine {
            account_id: account,
            name: machine_name("max"),
            first_seen: instant(2_000),
            last_seen: instant(3_000),
        })
        .await
        .expect("machine");
    database
        .store
        .upsert_machine(Machine {
            account_id: account,
            name: machine_name("max"),
            first_seen: instant(1_000),
            last_seen: instant(2_500),
        })
        .await
        .expect("machine update");
    let machines = database
        .store
        .list_machines(account)
        .await
        .expect("machines");
    assert_eq!(machines.len(), 1);
    assert_eq!(machines[0].first_seen, instant(1_000));
    assert_eq!(machines[0].last_seen, instant(3_000));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn staleness_is_vendor_aware_and_current_windows_include_provenance() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    let reset = 9_000_000;

    database
        .store
        .append_usage_snapshot(snapshot(
            account,
            "claude",
            "midnight",
            Vendor::AnthropicOauth,
            10_000,
            vec![
                window(QuotaWindowKind::FiveHour, 50.0, reset),
                window(QuotaWindowKind::RollingSevenDay, 52.0, reset),
            ],
        ))
        .await
        .expect("initial snapshot");
    database
        .store
        .append_usage_snapshot(snapshot(
            account,
            "claude",
            "midnight",
            Vendor::AnthropicOauth,
            20_000,
            vec![
                window(QuotaWindowKind::FiveHour, 20.0, reset),
                window(QuotaWindowKind::RollingSevenDay, 16.0, reset),
            ],
        ))
        .await
        .expect("decreasing snapshot");
    let current = database
        .store
        .current_usage(account, profile_name("claude"))
        .await
        .expect("current");
    let five = current
        .iter()
        .find(|item| item.window.kind == QuotaWindowKind::FiveHour)
        .expect("five hour");
    let rolling = current
        .iter()
        .find(|item| item.window.kind == QuotaWindowKind::RollingSevenDay)
        .expect("rolling");
    assert_close(five.window.used_percent.get(), 50.0);
    assert_close(rolling.window.used_percent.get(), 16.0);

    database
        .store
        .append_usage_snapshot(snapshot(
            account,
            "claude",
            "max",
            Vendor::AnthropicOauth,
            25_000,
            vec![window(QuotaWindowKind::FiveHour, 5.0, reset)],
        ))
        .await
        .expect("cross-machine regression is retained but rejected");
    let current = database
        .store
        .current_usage(account, profile_name("claude"))
        .await
        .expect("current after regression");
    assert_close(
        current
            .iter()
            .find(|item| item.window.kind == QuotaWindowKind::FiveHour)
            .expect("five hour")
            .window
            .used_percent
            .get(),
        50.0,
    );
    let five = current
        .iter()
        .find(|item| item.window.kind == QuotaWindowKind::FiveHour)
        .expect("five hour");
    assert_eq!(five.contributors.len(), 2);
    assert!(five.contributors.iter().any(|item| {
        item.machine.as_str() == "max" && !item.chosen && item.polled_at == instant(25_000)
    }));

    database
        .store
        .append_usage_snapshot(snapshot(
            account,
            "claude",
            "max",
            Vendor::AnthropicOauth,
            30_000,
            vec![window(QuotaWindowKind::FiveHour, 61.0, reset + 3_600_000)],
        ))
        .await
        .expect("new machine");
    let current = database
        .store
        .current_usage(account, profile_name("claude"))
        .await
        .expect("current");
    let five = current
        .iter()
        .find(|item| item.window.kind == QuotaWindowKind::FiveHour)
        .expect("five hour");
    assert_close(five.window.used_percent.get(), 61.0);
    assert_eq!(five.contributors.len(), 2);
    assert_eq!(
        five.contributors.iter().filter(|item| item.chosen).count(),
        1
    );
    assert_eq!(
        five.contributors
            .iter()
            .find(|item| item.chosen)
            .expect("winner")
            .machine
            .as_str(),
        "max"
    );

    database
        .store
        .append_usage_snapshot(snapshot(
            account,
            "codex",
            "max",
            Vendor::OpenaiCodex,
            40_000,
            vec![
                window(QuotaWindowKind::FiveHour, 40.0, reset),
                window(QuotaWindowKind::FixedWeekly, 70.0, reset),
            ],
        ))
        .await
        .expect("codex initial");
    database
        .store
        .append_usage_snapshot(snapshot(
            account,
            "codex",
            "midnight",
            Vendor::OpenaiCodex,
            50_000,
            vec![
                window(QuotaWindowKind::FiveHour, 5.0, reset),
                window(QuotaWindowKind::FixedWeekly, 10.0, reset),
            ],
        ))
        .await
        .expect("codex stale");
    let codex = database
        .store
        .current_usage(account, profile_name("codex"))
        .await
        .expect("codex current");
    assert_close(
        codex
            .iter()
            .find(|item| item.window.kind == QuotaWindowKind::FiveHour)
            .expect("codex five hour")
            .window
            .used_percent
            .get(),
        40.0,
    );
    assert_close(
        codex
            .iter()
            .find(|item| item.window.kind == QuotaWindowKind::FixedWeekly)
            .expect("codex weekly")
            .window
            .used_percent
            .get(),
        70.0,
    );
}

#[tokio::test]
async fn snapshots_are_append_only_and_typed_failures_preserve_null_windows() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    database
        .store
        .append_usage_snapshot(snapshot(
            account,
            "claude",
            "midnight",
            Vendor::AnthropicOauth,
            10_000,
            vec![window(QuotaWindowKind::FiveHour, 50.0, 100_000)],
        ))
        .await
        .expect("success");
    database
        .store
        .append_usage_snapshot(UsageSnapshot {
            account_id: account,
            profile: profile_name("claude"),
            machine: machine_name("midnight"),
            vendor: Vendor::AnthropicOauth,
            windows: Vec::new(),
            outcome: CollectionOutcome::AuthenticationFailed {
                code: "expired".to_owned(),
            },
            polled_at: instant(20_000),
            reporter_version: None,
        })
        .await
        .expect("typed failure");
    let history = database
        .store
        .usage_history(account, profile_name("claude"), None, 10)
        .await
        .expect("history");
    assert_eq!(history.len(), 2);
    assert!(history[0].snapshot.windows.is_empty());
    assert!(matches!(
        history[0].snapshot.outcome,
        CollectionOutcome::AuthenticationFailed { .. }
    ));
    assert!(history[1].id < history[0].id);
}

#[tokio::test]
async fn context_and_token_upserts_are_idempotent_freshness_aware_and_scoped() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let one = account_id(1);
    let two = account_id(2);
    database
        .store
        .upsert_context_session(context(one, "claude", "midnight", "s1", 100_000, 20_000))
        .await
        .expect("context");
    database
        .store
        .upsert_context_session(context(one, "claude", "midnight", "s1", 10_000, 10_000))
        .await
        .expect("stale context ignored");
    assert_eq!(
        database
            .store
            .list_context_sessions(one, Some(profile_name("claude")))
            .await
            .expect("context")[0]
            .context_tokens,
        Some(100_000)
    );
    assert!(
        database
            .store
            .list_context_sessions(two, Some(profile_name("claude")))
            .await
            .expect("other context")
            .is_empty()
    );

    database
        .store
        .upsert_token_grain(token(one, "claude", "midnight", "s1", 10))
        .await
        .expect("tokens");
    database
        .store
        .upsert_token_grain(token(one, "claude", "midnight", "s1", 25))
        .await
        .expect("token update");
    let tokens = database
        .store
        .list_token_grains(
            one,
            Some(profile_name("claude")),
            Some("2026-08-01".to_owned()),
            10,
        )
        .await
        .expect("tokens");
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].tokens_in, 25);
    assert!(
        database
            .store
            .list_token_grains(two, None, None, 10)
            .await
            .expect("other tokens")
            .is_empty()
    );
}

fn pricing(key: &str, input: f64) -> PricingRule {
    PricingRule {
        key: key.to_owned(),
        vendor: Vendor::AnthropicOauth,
        model_pattern: "claude-*".to_owned(),
        settings_match: BTreeMap::new(),
        input_per_million_usd: input,
        output_per_million_usd: 15.0,
        cache_write_5m_per_million_usd: 3.75,
        cache_write_1h_per_million_usd: 7.5,
        cache_read_per_million_usd: 0.3,
    }
}

#[tokio::test]
async fn pricing_defaults_and_overrides_upsert_without_cross_account_leakage() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    database
        .store
        .upsert_pricing_default(pricing("claude", 3.0))
        .await
        .expect("default");
    database
        .store
        .upsert_pricing_default(pricing("claude", 4.0))
        .await
        .expect("default update");
    assert_close(
        database
            .store
            .list_pricing_defaults()
            .await
            .expect("defaults")[0]
            .input_per_million_usd,
        4.0,
    );
    database
        .store
        .upsert_pricing_override(account_id(1), pricing("claude", 1.0))
        .await
        .expect("override one");
    database
        .store
        .upsert_pricing_override(account_id(2), pricing("claude", 2.0))
        .await
        .expect("override two");
    assert_close(
        database
            .store
            .list_pricing_overrides(account_id(1))
            .await
            .expect("one")[0]
            .input_per_million_usd,
        1.0,
    );
    assert_close(
        database
            .store
            .list_pricing_overrides(account_id(2))
            .await
            .expect("two")[0]
            .input_per_million_usd,
        2.0,
    );
    assert!(
        database
            .store
            .delete_pricing_override(account_id(1), "claude".to_owned())
            .await
            .expect("delete own override")
    );
    assert!(
        !database
            .store
            .delete_pricing_override(account_id(1), "claude".to_owned())
            .await
            .expect("missing override")
    );
    assert!(
        database
            .store
            .list_pricing_overrides(account_id(1))
            .await
            .expect("one after delete")
            .is_empty()
    );
    assert_eq!(
        database
            .store
            .list_pricing_defaults()
            .await
            .expect("seeded default survives")
            .len(),
        1
    );
    assert_close(
        database
            .store
            .list_pricing_overrides(account_id(2))
            .await
            .expect("other account survives")[0]
            .input_per_million_usd,
        2.0,
    );
    assert_eq!(
        database
            .store
            .delete_pricing_override(account_id(1), "../claude".to_owned())
            .await
            .expect_err("invalid key")
            .kind(),
        PulseErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn alert_cooldown_and_mutations_use_typed_account_scoped_instants() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    let subscription = database
        .store
        .create_alert_subscription(
            AlertSubscription {
                account_id: account,
                profile: profile_name("claude"),
                alert_type: AlertType::FiveHourThreshold,
                threshold: Some(Percent::new(80.0).expect("threshold")),
                cooldown_minutes: 30,
                delivery: None,
                enabled: true,
            },
            instant(1_000),
        )
        .await
        .expect("subscription");
    let event = |at| AlertEventInput {
        account_id: account,
        subscription_id: subscription.id,
        profile: profile_name("claude"),
        alert_type: AlertType::FiveHourThreshold,
        message: "Usage crossed 80 percent".to_owned(),
        current_value: Some(Percent::new(81.0).expect("current")),
        threshold: Some(Percent::new(80.0).expect("threshold")),
        triggered_at: instant(at),
    };
    let first = database
        .store
        .record_alert_if_due(event(10_000))
        .await
        .expect("first")
        .expect("inserted");
    assert!(
        database
            .store
            .record_alert_if_due(event(10_000 + 29 * 60_000))
            .await
            .expect("cooldown")
            .is_none()
    );
    assert!(
        database
            .store
            .record_alert_if_due(event(10_000 + 30 * 60_000))
            .await
            .expect("after cooldown")
            .is_some()
    );
    assert!(
        !database
            .store
            .acknowledge_alert(account_id(2), first.id)
            .await
            .expect("wrong account ack")
    );
    assert!(
        database
            .store
            .acknowledge_alert(account, first.id)
            .await
            .expect("ack")
    );
    assert_eq!(
        database
            .store
            .list_alert_events(account, Some(true))
            .await
            .expect("acked")
            .len(),
        1
    );
    assert!(
        !database
            .store
            .delete_alert_subscription(account_id(2), subscription.id)
            .await
            .expect("wrong delete")
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn alerts_fail_closed_on_stored_shape_and_replies_acknowledge_atomically() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    let subscription = database
        .store
        .create_alert_subscription(
            AlertSubscription {
                account_id: account,
                profile: profile_name("claude"),
                alert_type: AlertType::FiveHourThreshold,
                threshold: Some(Percent::new(80.0).expect("threshold")),
                cooldown_minutes: 30,
                delivery: None,
                enabled: true,
            },
            instant(1_000),
        )
        .await
        .expect("subscription");
    let event = |current: Option<f64>, threshold: Option<f64>| AlertEventInput {
        account_id: account,
        subscription_id: subscription.id,
        profile: profile_name("claude"),
        alert_type: AlertType::FiveHourThreshold,
        message: "Usage threshold reached".to_owned(),
        current_value: current.map(|value| Percent::new(value).expect("current")),
        threshold: threshold.map(|value| Percent::new(value).expect("threshold")),
        triggered_at: instant(10_000),
    };
    assert_eq!(
        database
            .store
            .record_alert_if_due(event(Some(90.0), Some(70.0)))
            .await
            .expect_err("stored threshold mismatch")
            .kind(),
        PulseErrorKind::Conflict
    );
    assert_eq!(
        database
            .store
            .record_alert_if_due(event(Some(79.0), Some(80.0)))
            .await
            .expect_err("below threshold")
            .kind(),
        PulseErrorKind::InvalidInput
    );
    let alert = database
        .store
        .record_alert_if_due(event(Some(90.0), Some(80.0)))
        .await
        .expect("valid event")
        .expect("inserted");
    assert!(
        database
            .store
            .reply_to_alert(AlertReplyInput {
                account_id: account_id(2),
                event_id: alert.id,
                message: "wrong account".to_owned(),
                replied_at: instant(20_000),
            })
            .await
            .expect("scoped miss")
            .is_none()
    );
    let reply = database
        .store
        .reply_to_alert(AlertReplyInput {
            account_id: account,
            event_id: alert.id,
            message: "Investigating now\nNo credentials included.".to_owned(),
            replied_at: instant(20_000),
        })
        .await
        .expect("reply")
        .expect("event exists");
    assert_eq!(reply.event_id, alert.id);
    assert_eq!(
        database
            .store
            .list_alert_events(account, Some(true))
            .await
            .expect("acknowledged")
            .len(),
        1
    );
    assert_eq!(
        database
            .store
            .list_alert_replies(account, alert.id)
            .await
            .expect("replies"),
        vec![reply]
    );
    assert!(
        database
            .store
            .list_alert_replies(account_id(2), alert.id)
            .await
            .expect("other account")
            .is_empty()
    );
}

#[tokio::test]
async fn reset_resume_jobs_are_deduped_leased_scoped_and_cancellable() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    let input = ResetResumeInput {
        account_id: account,
        profile: profile_name("claude"),
        resets_at: instant(100_000),
        scheduled_at: instant(10_000),
    };
    let first = database
        .store
        .schedule_reset_resume(input.clone(), ResetResumeLimits::default())
        .await
        .expect("schedule");
    let duplicate = database
        .store
        .schedule_reset_resume(input, ResetResumeLimits::default())
        .await
        .expect("dedupe");
    assert_eq!(first.id, duplicate.id);
    assert_eq!(first.resume_at, instant(160_000));
    assert!(
        database
            .store
            .list_pending_reset_resumes(account_id(2), instant(200_000), 10)
            .await
            .expect("other account")
            .is_empty()
    );
    assert!(
        database
            .store
            .claim_due_reset_resumes(account, instant(159_999), instant(170_000), 10)
            .await
            .expect("not due")
            .is_empty()
    );
    let claimed = database
        .store
        .claim_due_reset_resumes(account, instant(160_000), instant(170_000), 10)
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].attempts, 1);
    assert!(
        database
            .store
            .claim_due_reset_resumes(account, instant(160_001), instant(180_000), 10)
            .await
            .expect("leased")
            .is_empty()
    );
    assert!(
        !database
            .store
            .complete_reset_resume(account_id(2), first.id, instant(170_000))
            .await
            .expect("wrong account complete")
    );
    assert!(
        database
            .store
            .complete_reset_resume(account, first.id, instant(170_000))
            .await
            .expect("complete")
    );

    database
        .store
        .schedule_reset_resume(
            ResetResumeInput {
                account_id: account,
                profile: profile_name("claude"),
                resets_at: instant(300_000),
                scheduled_at: instant(200_000),
            },
            ResetResumeLimits::default(),
        )
        .await
        .expect("second schedule");
    assert_eq!(
        database
            .store
            .cancel_reset_resumes(account_id(2), profile_name("claude"), instant(210_000))
            .await
            .expect("wrong account cancel"),
        0
    );
    assert_eq!(
        database
            .store
            .cancel_reset_resumes(account, profile_name("claude"), instant(210_000))
            .await
            .expect("cancel"),
        1
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn reported_profiles_and_ingest_replays_are_transactional_and_safe() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    let reported = Profile {
        account_id: account,
        name: profile_name("remote-claude"),
        vendor: Vendor::AnthropicOauth,
        config_dir: None,
        poll_interval_minutes: 20,
        monthly_budget_usd: None,
        api_key_env: None,
        api_key_file: None,
        refresh: RefreshPolicy::Never,
        hidden: false,
        origin: atmux::pulse::ProfileOrigin::Reported,
    };
    let batch = IngestBatch {
        profiles: vec![reported.clone()],
        snapshots: Vec::new(),
        token_grains: Vec::new(),
        context_sessions: Vec::new(),
        gemini_quotas: Vec::new(),
    };
    let replay = IngestReplay {
        request_id: "request-1".to_owned(),
        payload_fingerprint: "c".repeat(64),
        received_at: instant(10_000),
    };
    let limits = IngestLimits::default();
    let first = database
        .store
        .ingest_batch_once(
            account,
            machine_name("midnight"),
            batch.clone(),
            limits,
            replay.clone(),
        )
        .await
        .expect("first ingest");
    assert!(!first.replayed);
    let second = database
        .store
        .ingest_batch_once(
            account,
            machine_name("midnight"),
            batch.clone(),
            limits,
            replay.clone(),
        )
        .await
        .expect("idempotent replay");
    assert!(second.replayed);
    assert_eq!(second.result, first.result);
    let mut conflict = replay;
    conflict.payload_fingerprint = "d".repeat(64);
    assert_eq!(
        database
            .store
            .ingest_batch_once(account, machine_name("midnight"), batch, limits, conflict,)
            .await
            .expect_err("fingerprint conflict")
            .kind(),
        PulseErrorKind::Conflict
    );
    assert_eq!(
        database
            .store
            .get_profile(account, reported.name.clone())
            .await
            .expect("get reported")
            .expect("reported profile")
            .origin,
        atmux::pulse::ProfileOrigin::Reported
    );

    let local_before = database
        .store
        .get_profile(account, profile_name("claude"))
        .await
        .expect("local")
        .expect("local profile");
    let shadow = Profile {
        origin: atmux::pulse::ProfileOrigin::Reported,
        config_dir: None,
        api_key_env: None,
        api_key_file: None,
        refresh: RefreshPolicy::Never,
        poll_interval_minutes: 99,
        ..local_before.clone()
    };
    database
        .store
        .ingest_batch(
            account,
            machine_name("midnight"),
            IngestBatch {
                profiles: vec![shadow],
                ..IngestBatch::default()
            },
            limits,
        )
        .await
        .expect("reported shadow");
    assert_eq!(
        database
            .store
            .get_profile(account, profile_name("claude"))
            .await
            .expect("local after")
            .expect("local profile"),
        local_before
    );
}

#[tokio::test]
async fn ingest_tokens_are_hash_only_and_all_mutations_are_scoped() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let token = IngestToken {
        id: 7,
        account_id: account_id(1),
        machine: machine_name("midnight"),
        token_hash: "a".repeat(64),
        created_at: instant(1_000),
        last_used_at: None,
        revoked_at: None,
    };
    database
        .store
        .insert_ingest_token(token)
        .await
        .expect("insert hash");
    assert!(
        database
            .store
            .get_ingest_token(account_id(2), 7)
            .await
            .expect("wrong-account lookup")
            .is_none()
    );
    assert_eq!(
        database
            .store
            .get_ingest_token(account_id(1), 7)
            .await
            .expect("token lookup")
            .expect("token")
            .token_hash,
        "a".repeat(64)
    );
    assert!(
        !database
            .store
            .touch_ingest_token(account_id(2), 7, instant(2_000))
            .await
            .expect("wrong touch")
    );
    assert!(
        database
            .store
            .touch_ingest_token(account_id(1), 7, instant(2_000))
            .await
            .expect("touch")
    );
    assert!(
        !database
            .store
            .revoke_ingest_token(account_id(2), 7, instant(3_000))
            .await
            .expect("wrong revoke")
    );
    assert!(
        database
            .store
            .revoke_ingest_token(account_id(1), 7, instant(3_000))
            .await
            .expect("revoke")
    );
    let listed = database
        .store
        .list_ingest_tokens(account_id(1))
        .await
        .expect("tokens");
    assert_eq!(listed[0].token_hash, "a".repeat(64));
    assert_eq!(listed[0].last_used_at, Some(instant(2_000)));
    assert_eq!(listed[0].revoked_at, Some(instant(3_000)));

    let invalid = IngestToken {
        id: 8,
        account_id: account_id(1),
        machine: machine_name("midnight"),
        token_hash: "plaintext-token".to_owned(),
        created_at: instant(1_000),
        last_used_at: None,
        revoked_at: None,
    };
    assert!(database.store.insert_ingest_token(invalid).await.is_err());
}

#[tokio::test]
async fn gemini_is_latest_only_scoped_and_import_provenance_is_idempotent() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let quota = |account, remaining, collected| GeminiQuota {
        account_id: account,
        model_id: "gemini-2.5-pro".to_owned(),
        remaining_fraction: Fraction::new(remaining).expect("fraction"),
        remaining_amount: None,
        resets_at: None,
        collected_at: instant(collected),
    };
    database
        .store
        .upsert_gemini_quota(quota(account_id(1), 0.4, 2_000))
        .await
        .expect("quota");
    database
        .store
        .upsert_gemini_quota(quota(account_id(1), 0.9, 1_000))
        .await
        .expect("older ignored");
    database
        .store
        .upsert_gemini_quota(quota(account_id(2), 0.7, 3_000))
        .await
        .expect("other quota");
    assert_close(
        database
            .store
            .list_gemini_quotas(account_id(1))
            .await
            .expect("quota")[0]
            .remaining_fraction
            .get(),
        0.4,
    );
    assert_close(
        database
            .store
            .list_gemini_quotas(account_id(2))
            .await
            .expect("other quota")[0]
            .remaining_fraction
            .get(),
        0.7,
    );

    let provenance = ImportProvenance {
        account_id: account_id(1),
        source_fingerprint: "b".repeat(64),
        source_table: "usage_snapshots".to_owned(),
        source_row_id: "42".to_owned(),
        target_key: "1/claude/42".to_owned(),
        payload_fingerprint: "1".repeat(64),
        imported_at: instant(4_000),
    };
    assert!(
        database
            .store
            .record_import(provenance.clone())
            .await
            .expect("first import")
    );
    let other_account = ImportProvenance {
        account_id: account_id(2),
        ..provenance.clone()
    };
    assert!(
        database
            .store
            .record_import(other_account)
            .await
            .expect("same source row in another account")
    );
    assert!(
        !database
            .store
            .record_import(provenance)
            .await
            .expect("duplicate within account")
    );
}

#[tokio::test]
async fn imported_snapshot_and_provenance_commit_atomically_once() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    let provenance = ImportProvenance {
        account_id: account,
        source_fingerprint: "c".repeat(64),
        source_table: "usage_snapshots".to_owned(),
        source_row_id: "atomic-42".to_owned(),
        target_key: "1/claude/atomic-42".to_owned(),
        payload_fingerprint: "2".repeat(64),
        imported_at: instant(4_000),
    };
    let imported = snapshot(
        account,
        "claude",
        "midnight",
        Vendor::AnthropicOauth,
        5_000,
        vec![window(QuotaWindowKind::FiveHour, 25.0, 9_000_000)],
    );

    let mismatched = ImportProvenance {
        account_id: account_id(2),
        ..provenance.clone()
    };
    let error = database
        .store
        .append_imported_usage_snapshot_once(mismatched, imported.clone())
        .await
        .expect_err("cross-account import must fail before provenance is recorded");
    assert_eq!(error.kind(), PulseErrorKind::InvalidInput);

    assert!(
        database
            .store
            .append_imported_usage_snapshot_once(provenance.clone(), imported.clone())
            .await
            .expect("first atomic import")
    );
    assert!(
        !database
            .store
            .append_imported_usage_snapshot_once(provenance, imported)
            .await
            .expect("exact replay")
    );
    let history = database
        .store
        .usage_history(account, profile_name("claude"), None, 10)
        .await
        .expect("imported history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].snapshot.polled_at, instant(5_000));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn ingest_caps_allow_at_cap_updates_and_rollback_the_whole_batch() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    let machine = machine_name("midnight");
    let limits = IngestLimits {
        max_rows_per_request: 10,
        max_profiles: 4,
        max_usage_snapshots: 1,
        max_token_rows: 1,
        max_context_sessions: 1,
        max_gemini_models: 1,
    };
    let first = IngestBatch {
        profiles: Vec::new(),
        snapshots: vec![snapshot(
            account,
            "claude",
            "midnight",
            Vendor::AnthropicOauth,
            9_000,
            vec![window(QuotaWindowKind::FiveHour, 1.0, 100_000)],
        )],
        token_grains: vec![token(account, "claude", "midnight", "s1", 10)],
        context_sessions: vec![context(account, "claude", "midnight", "s1", 10, 10_000)],
        gemini_quotas: vec![GeminiQuota {
            account_id: account,
            model_id: "gemini-pro".to_owned(),
            remaining_fraction: Fraction::new(0.5).expect("fraction"),
            remaining_amount: None,
            resets_at: None,
            collected_at: instant(10_000),
        }],
    };
    database
        .store
        .ingest_batch(account, machine.clone(), first, limits)
        .await
        .expect("initial ingest");
    let at_cap_update = IngestBatch {
        token_grains: vec![token(account, "claude", "midnight", "s1", 99)],
        context_sessions: vec![context(account, "claude", "midnight", "s1", 99, 20_000)],
        ..IngestBatch::default()
    };
    database
        .store
        .ingest_batch(account, machine.clone(), at_cap_update, limits)
        .await
        .expect("at-cap updates are legal");
    assert_eq!(
        database
            .store
            .list_token_grains(account, None, None, 10)
            .await
            .expect("tokens")[0]
            .tokens_in,
        99
    );

    let history_before = database
        .store
        .usage_history(account, profile_name("claude"), None, 10)
        .await
        .expect("history")
        .len();
    let snapshot_over_cap = IngestBatch {
        snapshots: vec![snapshot(
            account,
            "claude",
            "midnight",
            Vendor::AnthropicOauth,
            30_000,
            vec![window(QuotaWindowKind::FiveHour, 1.0, 100_000)],
        )],
        ..IngestBatch::default()
    };
    assert!(
        database
            .store
            .ingest_batch(account, machine.clone(), snapshot_over_cap, limits)
            .await
            .is_err()
    );
    assert_eq!(
        database
            .store
            .usage_history(account, profile_name("claude"), None, 10)
            .await
            .expect("history after rollback")
            .len(),
        history_before
    );

    let token_over_cap = IngestBatch {
        token_grains: vec![token(account, "claude", "midnight", "s2", 1)],
        ..IngestBatch::default()
    };
    assert!(
        database
            .store
            .ingest_batch(account, machine.clone(), token_over_cap, limits)
            .await
            .is_err()
    );

    let cross_account = IngestBatch {
        token_grains: vec![token(account_id(2), "claude", "midnight", "s1", 1)],
        ..IngestBatch::default()
    };
    let error = database
        .store
        .ingest_batch(account, machine, cross_account, limits)
        .await
        .expect_err("scope rejected");
    assert_eq!(error.kind(), PulseErrorKind::Conflict);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn retention_sweeps_live_state_and_downsamples_snapshots_transactionally() {
    let database = TestStore::new().await;
    seed(database.store.as_ref()).await;
    let account = account_id(1);
    let day_ms = 24_i64 * 60 * 60 * 1_000;
    let hour_ms = 60_i64 * 60 * 1_000;
    let now = 200 * day_ms;

    database
        .store
        .upsert_context_session(context(
            account,
            "claude",
            "midnight",
            "old",
            10,
            now - 2 * day_ms,
        ))
        .await
        .expect("old context");
    database
        .store
        .upsert_context_session(context(
            account,
            "claude",
            "midnight",
            "fresh",
            10,
            now - hour_ms,
        ))
        .await
        .expect("fresh context");

    let snapshot_times = [
        now - 100 * day_ms,
        now - 100 * day_ms + 1_000,
        now - 100 * day_ms + 2_000,
        now - 10 * day_ms,
        now - 10 * day_ms + 1_000,
        now - day_ms,
        now - day_ms + 1_000,
    ];
    for (index, polled) in snapshot_times.into_iter().enumerate() {
        database
            .store
            .append_usage_snapshot(snapshot(
                account,
                "claude",
                "midnight",
                Vendor::AnthropicOauth,
                polled,
                vec![window(
                    QuotaWindowKind::RollingSevenDay,
                    10.0 + f64::from(u32::try_from(index).expect("small index")),
                    now + day_ms,
                )],
            ))
            .await
            .expect("snapshot");
    }

    let subscription = database
        .store
        .create_alert_subscription(
            AlertSubscription {
                account_id: account,
                profile: profile_name("claude"),
                alert_type: AlertType::AuthenticationFailure,
                threshold: None,
                cooldown_minutes: 1,
                delivery: None,
                enabled: true,
            },
            instant(1_000),
        )
        .await
        .expect("subscription");
    for triggered_at in [now - 190 * day_ms, now - 10 * day_ms] {
        database
            .store
            .record_alert_if_due(AlertEventInput {
                account_id: account,
                subscription_id: subscription.id,
                profile: profile_name("claude"),
                alert_type: AlertType::AuthenticationFailure,
                message: "Authentication needs attention".to_owned(),
                current_value: None,
                threshold: None,
                triggered_at: instant(triggered_at),
            })
            .await
            .expect("alert")
            .expect("inserted");
    }

    let result = database
        .store
        .apply_retention(instant(now), 1, 180, 7, 90)
        .await
        .expect("retention");
    assert_eq!(result.context_sessions, 1);
    assert_eq!(result.alert_events, 1);
    assert_eq!(result.usage_snapshots, 3);
    assert_eq!(result.usage_windows, 3);
    assert_eq!(
        database
            .store
            .list_context_sessions(account, None)
            .await
            .expect("contexts")
            .len(),
        1
    );
    assert_eq!(
        database
            .store
            .usage_history(account, profile_name("claude"), None, 10)
            .await
            .expect("history")
            .len(),
        4
    );
    assert_eq!(
        database
            .store
            .list_alert_events(account, None)
            .await
            .expect("events")
            .len(),
        1
    );
    assert_eq!(
        database.store.integrity_check().await.expect("integrity"),
        "ok"
    );
}
