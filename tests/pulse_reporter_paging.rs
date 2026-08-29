#![cfg(feature = "pulse")]

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use atmux::pulse::{
    Account, AccountId, AgentSettings, CollectionOutcome, Instant, Machine, MachineName, Percent,
    Profile, ProfileName, ProfileOrigin, QuotaWindow, QuotaWindowKind, RefreshPolicy,
    UsageSnapshot, Vendor,
    collect::SecretRef,
    ingest::{PUSH_VERSION, PushBatch, PushEnvelope, REPORTER_VERSION},
    reporter::{
        AccountReporterOutcome, PulseReporter, ReporterBackoff, ReporterFuture, ReporterRequest,
        ReporterResponse, ReporterTransport, StoreReporterCoordinator,
    },
    store::{
        MAX_REPORTER_PENDING_CHUNKS, ReporterCursorState, ReporterPendingChunk,
        ReporterPendingDraft, ReporterPendingPage, ReporterStreamKind, SqliteStore, Store,
    },
};
use tokio::sync::watch;

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt as _;

const LOCAL_ROWS: usize = 10_050;
const REMOTE_ROWS: usize = 15_000;

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "atmux-pulse-reporter-{}-{nonce}",
            std::process::id()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(&path).expect("private test root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct CapturingTransport {
    calls: AtomicUsize,
    fail_at: Mutex<Option<usize>>,
    accepted: Mutex<Vec<Vec<u8>>>,
}

impl CapturingTransport {
    fn failing_at(call: usize) -> Self {
        Self {
            fail_at: Mutex::new(Some(call)),
            ..Self::default()
        }
    }

    fn envelopes(&self) -> Vec<PushEnvelope> {
        self.accepted
            .lock()
            .expect("accepted requests")
            .iter()
            .map(|body| serde_json::from_slice(body).expect("push envelope"))
            .collect()
    }
}

impl ReporterTransport for CapturingTransport {
    fn send(&self, request: ReporterRequest) -> ReporterFuture<ReporterResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        let fail = self
            .fail_at
            .lock()
            .expect("failure point")
            .is_some_and(|failure| failure == call);
        if !fail {
            self.accepted
                .lock()
                .expect("accepted requests")
                .push(request.body().to_vec());
        }
        Box::pin(async move {
            Ok(ReporterResponse {
                status: if fail { 500 } else { 200 },
                retry_after: None,
            })
        })
    }
}

fn account() -> AccountId {
    AccountId::new(1).expect("account")
}

fn machine(value: &str) -> MachineName {
    MachineName::new(value).expect("machine")
}

fn profile() -> ProfileName {
    ProfileName::new("claude-max").expect("profile")
}

fn usage_snapshot(polled_at: i64) -> UsageSnapshot {
    UsageSnapshot {
        account_id: account(),
        profile: profile(),
        machine: machine("midnight"),
        vendor: Vendor::AnthropicOauth,
        windows: vec![QuotaWindow {
            kind: QuotaWindowKind::FiveHour,
            used_percent: Percent::new(10.0).expect("percent"),
            resets_at: Instant::from_epoch_millis(polled_at + 60_000).expect("reset"),
        }],
        outcome: CollectionOutcome::Success,
        polled_at: Instant::from_epoch_millis(polled_at).expect("poll"),
        reporter_version: None,
    }
}

fn usage_pending_draft(
    expected: ReporterCursorState,
    next: ReporterCursorState,
    snapshots: Vec<UsageSnapshot>,
) -> ReporterPendingDraft {
    let request_id = "push-crash-window-proof".to_owned();
    let rows = snapshots.len();
    let body = PushEnvelope {
        version: PUSH_VERSION,
        request_id: request_id.clone(),
        reporter_version: REPORTER_VERSION.to_owned(),
        account_id: Some(account()),
        machine: Some(machine("midnight")),
        batch: PushBatch {
            snapshots,
            ..PushBatch::default()
        },
    }
    .encode()
    .expect("encode pending page");
    ReporterPendingDraft {
        kind: ReporterStreamKind::Usage,
        expected,
        next,
        chunks: vec![ReporterPendingChunk {
            request_id,
            body,
            rows,
        }],
    }
}

async fn seed_store(store: &dyn Store) {
    store
        .upsert_account(Account {
            id: account(),
            identity: "reporter@example.test".to_owned(),
            display_name: None,
        })
        .await
        .expect("account");
    for name in [machine("midnight"), machine("max")] {
        store
            .upsert_machine(Machine {
                account_id: account(),
                name,
                first_seen: Instant::from_epoch_millis(1).expect("first seen"),
                last_seen: Instant::from_epoch_millis(2).expect("last seen"),
            })
            .await
            .expect("machine");
    }
    store
        .upsert_profile(Profile {
            account_id: account(),
            name: profile(),
            vendor: Vendor::AnthropicOauth,
            config_dir: Some(PathBuf::from("/private/local-only")),
            poll_interval_minutes: 15,
            monthly_budget_usd: None,
            api_key_env: None,
            api_key_file: None,
            refresh: RefreshPolicy::InMemory,
            hidden: false,
            origin: ProfileOrigin::Local,
        })
        .await
        .expect("profile");
}

fn bulk_seed(path: &Path) {
    let mut connection = rusqlite::Connection::open(path).expect("bulk database");
    let transaction = connection.transaction().expect("bulk transaction");
    let vendor = serde_json::to_string(&Vendor::AnthropicOauth).expect("vendor");
    let outcome = serde_json::to_string(&atmux::pulse::CollectionOutcome::Success)
        .expect("collection outcome");
    let window = serde_json::to_string(&atmux::pulse::QuotaWindowKind::FiveHour).expect("window");
    let settings = AgentSettings::default();
    let settings_json = serde_json::to_string(&settings).expect("settings");
    let settings_hash = settings.sha256().expect("settings hash");
    let source = serde_json::to_string(&atmux::pulse::TokenSource::Local).expect("source");
    for (machine, count, base) in [
        ("midnight", LOCAL_ROWS, 10_000_i64),
        ("max", REMOTE_ROWS, 1_000_000_i64),
    ] {
        for index in 0..count {
            let index = i64::try_from(index).expect("row index");
            transaction
                .execute(
                    "INSERT INTO usage_snapshots \
                     (account_id,profile,machine,vendor_json,outcome_json,polled_at_ms) \
                     VALUES (1,'claude-max',?1,?2,?3,?4)",
                    rusqlite::params![machine, vendor, outcome, base + index],
                )
                .expect("usage snapshot");
            let snapshot_id = transaction.last_insert_rowid();
            transaction
                .execute(
                    "INSERT INTO usage_windows \
                     (snapshot_id,kind_json,used_percent,resets_at_ms,accepted) \
                     VALUES (?1,?2,10.0,2000000,1)",
                    rusqlite::params![snapshot_id, window],
                )
                .expect("usage window");
            transaction
                .execute(
                    "INSERT INTO token_usage \
                     (account_id,profile,machine,session_id,model,settings_hash,settings_json,day, \
                      tokens_in,tokens_out,cache_write_5m,cache_write_1h,cache_read,source_json, \
                      updated_at_ms) VALUES \
                     (1,'claude-max',?1,?2,'claude-opus-5',?3,?4,'2026-08-08', \
                      ?5,2,3,4,5,?6,?7)",
                    rusqlite::params![
                        machine,
                        format!("s{index:05}"),
                        settings_hash,
                        settings_json,
                        index + 1,
                        source,
                        base + index,
                    ],
                )
                .expect("token grain");
        }
    }
    transaction.commit().expect("commit bulk rows");
}

fn reporter(token_path: &Path, transport: Arc<dyn ReporterTransport>) -> Arc<PulseReporter> {
    Arc::new(
        PulseReporter::new(
            "http://127.0.0.1:7345/api/v1/pulse/ingest".to_owned(),
            SecretRef::File {
                path: token_path.to_path_buf(),
            },
            transport,
            ReporterBackoff {
                max_attempts: 1,
                jitter_percent: 0,
                ..ReporterBackoff::default()
            },
        )
        .expect("reporter"),
    )
}

fn reported_rows(envelopes: &[PushEnvelope]) -> (Vec<i64>, Vec<(String, u64)>) {
    let usage = envelopes
        .iter()
        .flat_map(|envelope| &envelope.batch.snapshots)
        .map(|snapshot| {
            assert_eq!(snapshot.machine, machine("midnight"));
            snapshot.polled_at.epoch_millis()
        })
        .collect();
    let tokens = envelopes
        .iter()
        .flat_map(|envelope| &envelope.batch.token_grains)
        .map(|grain| {
            assert_eq!(grain.machine, machine("midnight"));
            (grain.session_id.as_str().to_owned(), grain.tokens_in)
        })
        .collect();
    (usage, tokens)
}

async fn report_with_timeout(
    coordinator: &StoreReporterCoordinator,
    completed_at: i64,
    cancellation: &mut watch::Receiver<bool>,
) -> Vec<AccountReporterOutcome> {
    tokio::time::timeout(
        Duration::from_secs(10),
        coordinator.report_completed(
            Instant::from_epoch_millis(completed_at).expect("completion"),
            cancellation,
        ),
    )
    .await
    .expect("report timeout")
}

async fn prepare_crash_window_page(
    store: &dyn Store,
    destination: &str,
) -> (ReporterPendingPage, ReporterCursorState) {
    for polled_at in [10_000, 20_000] {
        store
            .append_usage_snapshot(usage_snapshot(polled_at))
            .await
            .expect("append crash-window source row");
    }
    let expected = store
        .load_reporter_cursor(account(), machine("midnight"), destination.to_owned())
        .await
        .expect("initialize reporter cursor");
    let rows = store
        .local_reporter_usage_page(account(), machine("midnight"), 0, 500)
        .await
        .expect("load source page");
    let mut next = expected.clone();
    next.usage_after_id = rows.last().expect("source row").id;
    let draft = usage_pending_draft(
        expected,
        next.clone(),
        rows.into_iter().map(|row| row.snapshot).collect(),
    );
    let pending = store
        .prepare_reporter_pending(
            account(),
            machine("midnight"),
            destination.to_owned(),
            draft,
        )
        .await
        .expect("prepare exact pending page");
    (pending, next)
}

fn delete_prepared_source(database_path: &Path) {
    let connection = rusqlite::Connection::open(database_path).expect("open source database");
    connection
        .execute(
            "DELETE FROM usage_snapshots WHERE account_id=1 AND machine='midnight'",
            [],
        )
        .expect("retention deletes prepared source");
}

#[tokio::test]
async fn accepted_page_replays_identically_after_crash_and_source_retention() {
    let root = TempRoot::new();
    let database_path = root.path().join("pulse.sqlite3");
    let concrete = Arc::new(SqliteStore::open(&database_path).await.expect("store"));
    seed_store(concrete.as_ref()).await;
    let destination =
        "reporter-v1-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let (prepared, next) = prepare_crash_window_page(concrete.as_ref(), destination).await;

    let accepted = &prepared.draft.chunks[0];
    let mut receiver_ledger = HashSet::new();
    assert!(receiver_ledger.insert(accepted.request_id.clone()));
    let accepted_body = accepted.body.clone();
    delete_prepared_source(&database_path);
    concrete
        .append_usage_snapshot(usage_snapshot(30_000))
        .await
        .expect("append source mutation after prepare");
    drop(concrete);

    let reopened = SqliteStore::open(&database_path)
        .await
        .expect("reopen after crash");
    let replay = reopened
        .load_reporter_pending(
            account(),
            machine("midnight"),
            destination.to_owned(),
            ReporterStreamKind::Usage,
        )
        .await
        .expect("load exact replay")
        .expect("pending replay");
    assert_eq!(replay, prepared);
    assert_eq!(replay.draft.chunks[0].body, accepted_body);
    assert!(!receiver_ledger.insert(replay.draft.chunks[0].request_id.clone()));
    assert_eq!(receiver_ledger.len(), 1, "receiver must not append twice");

    let committed = reopened
        .commit_reporter_pending(
            account(),
            machine("midnight"),
            destination.to_owned(),
            ReporterStreamKind::Usage,
            replay.id,
        )
        .await
        .expect("commit replayed page");
    assert_eq!(committed, next);
    assert!(
        reopened
            .load_reporter_pending(
                account(),
                machine("midnight"),
                destination.to_owned(),
                ReporterStreamKind::Usage,
            )
            .await
            .expect("load committed outbox")
            .is_none()
    );
    let later = reopened
        .local_reporter_usage_page(
            account(),
            machine("midnight"),
            committed.usage_after_id,
            500,
        )
        .await
        .expect("resume after retained page");
    assert_eq!(later.len(), 1, "newer source mutation remains reportable");
}

#[tokio::test]
async fn pending_chunk_cap_and_corrupted_body_fail_closed() {
    let root = TempRoot::new();
    let database_path = root.path().join("pulse.sqlite3");
    let store = SqliteStore::open(&database_path).await.expect("store");
    seed_store(&store).await;
    let destination =
        "reporter-v1-cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let (prepared, _) = prepare_crash_window_page(&store, destination).await;

    let other = "reporter-v1-dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    let expected = store
        .load_reporter_cursor(account(), machine("midnight"), other.to_owned())
        .await
        .expect("initialize bounded destination");
    let mut oversized = prepared.draft.clone();
    oversized.expected = expected.clone();
    oversized.next = expected;
    oversized.next.usage_after_id = prepared.draft.next.usage_after_id;
    oversized.chunks = vec![prepared.draft.chunks[0].clone(); MAX_REPORTER_PENDING_CHUNKS + 1];
    assert_eq!(
        store
            .prepare_reporter_pending(account(), machine("midnight"), other.to_owned(), oversized,)
            .await
            .expect_err("chunk cap must fail")
            .kind(),
        atmux::pulse::PulseErrorKind::InvalidInput
    );

    let connection = rusqlite::Connection::open(&database_path).expect("open outbox database");
    connection
        .execute(
            "UPDATE reporter_pending_chunks SET body=zeroblob(length(body)) \
             WHERE pending_id=?1",
            [prepared.id],
        )
        .expect("corrupt durable body");
    drop(connection);
    assert_eq!(
        store
            .load_reporter_pending(
                account(),
                machine("midnight"),
                destination.to_owned(),
                ReporterStreamKind::Usage,
            )
            .await
            .expect_err("corrupted body must fail closed")
            .kind(),
        atmux::pulse::PulseErrorKind::Storage
    );
}

#[tokio::test]
async fn reporter_pages_beyond_ten_thousand_resume_without_skip_or_remote_rows() {
    let root = TempRoot::new();
    let database_path = root.path().join("pulse.sqlite3");
    let token_path = root.path().join("report.token");
    fs::write(&token_path, "test-ingest-token").expect("report token");
    let concrete = Arc::new(SqliteStore::open(&database_path).await.expect("store"));
    seed_store(concrete.as_ref()).await;
    bulk_seed(&database_path);
    let store: Arc<dyn Store> = concrete.clone();

    let first_transport = Arc::new(CapturingTransport::failing_at(3));
    let first = StoreReporterCoordinator::new(
        Arc::clone(&store),
        Arc::from([account()]),
        machine("midnight"),
        reporter(&token_path, first_transport.clone()),
    );
    let (_shutdown, mut cancellation) = watch::channel(false);
    let first_outcome = report_with_timeout(&first, 10, &mut cancellation).await;
    assert!(first_outcome[0].result.is_err());
    let (first_usage, first_tokens) = reported_rows(&first_transport.envelopes());
    assert_eq!(first_usage.len(), 500);
    assert!(first_tokens.is_empty());
    drop(first);
    drop(store);
    drop(concrete);

    let reopened: Arc<dyn Store> = Arc::new(
        SqliteStore::open(&database_path)
            .await
            .expect("reopen after reporter restart"),
    );
    let resumed_transport = Arc::new(CapturingTransport::default());
    let resumed = StoreReporterCoordinator::new(
        Arc::clone(&reopened),
        Arc::from([account()]),
        machine("midnight"),
        reporter(&token_path, resumed_transport.clone()),
    );
    let resumed_outcome = report_with_timeout(&resumed, 20, &mut cancellation).await;
    assert!(resumed_outcome[0].result.is_ok());
    let (resumed_usage, resumed_tokens) = reported_rows(&resumed_transport.envelopes());
    assert_eq!(resumed_usage.len(), LOCAL_ROWS - 500);
    assert_eq!(resumed_tokens.len(), LOCAL_ROWS);
    let all_usage = first_usage
        .into_iter()
        .chain(resumed_usage)
        .collect::<HashSet<_>>();
    assert_eq!(all_usage.len(), LOCAL_ROWS);
    assert_eq!(
        resumed_tokens
            .iter()
            .map(|(session, _)| session)
            .collect::<HashSet<_>>()
            .len(),
        LOCAL_ROWS
    );

    let connection = rusqlite::Connection::open(&database_path).expect("mutate token row");
    connection
        .execute(
            "UPDATE token_usage SET tokens_in=999999 WHERE account_id=1 \
             AND machine='midnight' AND session_id='s00000'",
            [],
        )
        .expect("update early token key");
    drop(connection);
    let resync_transport = Arc::new(CapturingTransport::default());
    let resync = StoreReporterCoordinator::new(
        reopened,
        Arc::from([account()]),
        machine("midnight"),
        reporter(&token_path, resync_transport.clone()),
    );
    let resync_outcome = report_with_timeout(&resync, 30, &mut cancellation).await;
    assert!(resync_outcome[0].result.is_ok());
    let (resync_usage, resync_tokens) = reported_rows(&resync_transport.envelopes());
    assert!(
        resync_usage.is_empty(),
        "usage high-water must survive restart"
    );
    assert_eq!(resync_tokens.len(), LOCAL_ROWS);
    assert_eq!(
        resync_tokens
            .iter()
            .find(|(session, _)| session == "s00000")
            .expect("mutated early token")
            .1,
        999_999
    );
}
