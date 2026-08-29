#![cfg(feature = "pulse")]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use atmux::pulse::{
    AccountId, AgentSettings, CollectionOutcome, ContextSession, GeminiQuota, Instant, Machine,
    MachineName, Percent, Profile, ProfileName, ProfileOrigin, PulseAccountConfig, PulseConfig,
    PulseCredentialConfig, PulseDatabaseConfig, PulseProfileConfig, QuotaWindow, QuotaWindowKind,
    RefreshPolicy, SessionId, TokenGrain, TokenSource, UsageSnapshot, Vendor,
    collect::SecretRef,
    health::GaugeHealth,
    ingest::PushEnvelope,
    ops::{
        DoctorRequest, FullHistoryCollector, FullHistoryFuture, FullHistoryRows, LocalProfilePaths,
        PushOnceRequest, PushReportResult, doctor, open_doctor_store, push_once_with,
    },
    reporter::{
        PulseReporter, ReporterBackoff, ReporterFuture, ReporterRequest, ReporterResponse,
        ReporterTransport,
    },
    service::{
        Collected, CollectionFuture, CompletionFuture, PulseCollectors, PulseSink, StoreSink,
        TokenCollectionRequest,
    },
    store::{SqliteStore, Store},
    token::{TokenSourceGeneration, TokenTallyCursor},
};
use tokio::sync::watch;

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt as _;

fn sqlite_state(path: &Path) -> (Vec<(String, String)>, BTreeMap<String, i64>) {
    let connection = rusqlite::Connection::open(path).expect("inspect database");
    let schema = {
        let mut statement = connection
            .prepare(
                "SELECT name, COALESCE(sql, '') FROM sqlite_master \
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("schema statement");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("schema query")
            .collect::<Result<Vec<_>, _>>()
            .expect("schema rows")
    };
    let tables = {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("table statement");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("table query")
            .collect::<Result<Vec<_>, _>>()
            .expect("table rows")
    };
    let counts = tables
        .into_iter()
        .map(|table| {
            let quoted = table.replace('"', "\"\"");
            let count = connection
                .query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |row| {
                    row.get(0)
                })
                .expect("table count");
            (table, count)
        })
        .collect();
    (schema, counts)
}

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "atmux-pulse-ops-{label}-{}-{nonce}",
            std::process::id()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(&path).expect("temp root");
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

fn at(value: i64) -> Instant {
    Instant::from_epoch_millis(value).expect("instant")
}

fn account_id() -> AccountId {
    AccountId::new(7).expect("account id")
}

fn machine() -> MachineName {
    MachineName::new("midnight").expect("machine")
}

fn configured_profile(name: &str, vendor: Vendor, config_dir: &Path) -> PulseProfileConfig {
    PulseProfileConfig {
        name: name.to_owned(),
        vendor,
        config_dir: Some(config_dir.to_path_buf()),
        poll_interval_minutes: 15,
        monthly_budget_usd: None,
        api_key_env: None,
        api_key_file: None,
        refresh: RefreshPolicy::InMemory,
        hidden: false,
    }
}

fn config(config_dir: &Path, names: &[(&str, Vendor)]) -> PulseConfig {
    PulseConfig {
        credentials: PulseCredentialConfig {
            heal_config_dir: false,
            ..PulseCredentialConfig::default()
        },
        accounts: vec![PulseAccountConfig {
            id: account_id().get(),
            identity: "operator@example.test".to_owned(),
            display_name: Some("Operator".to_owned()),
            profiles: names
                .iter()
                .map(|(name, vendor)| configured_profile(name, *vendor, config_dir))
                .collect(),
        }],
        ..PulseConfig::default()
    }
}

fn local_paths(root: &Path) -> LocalProfilePaths {
    LocalProfilePaths::new(root.to_path_buf(), vec![root.join("bin")]).expect("local paths")
}

fn success_snapshot(profile: &Profile, polled_at: Instant, used: f64) -> UsageSnapshot {
    UsageSnapshot {
        account_id: profile.account_id,
        profile: profile.name.clone(),
        machine: machine(),
        vendor: profile.vendor,
        windows: vec![QuotaWindow {
            kind: QuotaWindowKind::FiveHour,
            used_percent: Percent::new(used).expect("percent"),
            resets_at: Instant::from_iso8601("2026-08-09T00:00:00Z").expect("reset"),
        }],
        outcome: CollectionOutcome::Success,
        polled_at,
        reporter_version: Some("test".to_owned()),
    }
}

fn failed_snapshot(
    profile: &Profile,
    polled_at: Instant,
    outcome: CollectionOutcome,
) -> UsageSnapshot {
    UsageSnapshot {
        account_id: profile.account_id,
        profile: profile.name.clone(),
        machine: machine(),
        vendor: profile.vendor,
        windows: Vec::new(),
        outcome,
        polled_at,
        reporter_version: Some("test".to_owned()),
    }
}

#[tokio::test]
async fn doctor_store_refuses_missing_database_and_preserves_schema_and_rows() {
    let root = TempRoot::new("doctor-store");
    let missing = root.path().join("missing.sqlite3");
    let mut missing_config = config(
        &root.path().join("profile"),
        &[("configured", Vendor::AnthropicOauth)],
    );
    missing_config.database = PulseDatabaseConfig {
        sqlite_path: Some(missing.clone()),
        postgres_url_env: None,
    };
    let error = open_doctor_store(&missing_config)
        .await
        .err()
        .expect("missing database must fail");
    assert_eq!(error.kind(), atmux::pulse::PulseErrorKind::NotFound);
    assert!(!missing.exists());

    let path = root.path().join("existing.sqlite3");
    let store = SqliteStore::open(&path)
        .await
        .expect("create current store");
    store
        .upsert_account(missing_config.accounts[0].account().expect("account"))
        .await
        .expect("seed account");
    drop(store);
    let before = sqlite_state(&path);
    let mut existing_config = missing_config;
    existing_config.database.sqlite_path = Some(path.clone());
    let doctor_store = open_doctor_store(&existing_config)
        .await
        .expect("open existing doctor store");
    assert!(doctor_store.schema_version().await.expect("schema") > 0);
    assert!(
        doctor_store
            .upsert_account(existing_config.accounts[0].account().expect("account"))
            .await
            .is_err(),
        "doctor store must reject writes"
    );
    drop(doctor_store);
    let after = sqlite_state(&path);
    assert_eq!(after, before);
}

#[cfg(unix)]
#[tokio::test]
async fn doctor_rejects_final_and_ancestor_symlinks_without_changing_target() {
    use std::os::unix::fs::symlink;

    let root = TempRoot::new("doctor-symlink");
    let actual = root.path().join("actual");
    let mut actual_builder = fs::DirBuilder::new();
    actual_builder.mode(0o700);
    actual_builder
        .create(&actual)
        .expect("actual database directory");
    let target = actual.join("pulse.sqlite3");
    let store = SqliteStore::open(&target).await.expect("target store");
    drop(store);
    let before = sqlite_state(&target);
    let mut pulse_config = config(
        &root.path().join("profile"),
        &[("configured", Vendor::AnthropicOauth)],
    );

    let final_link = root.path().join("final-link.sqlite3");
    symlink(&target, &final_link).expect("final database symlink");
    pulse_config.database.sqlite_path = Some(final_link);
    assert!(open_doctor_store(&pulse_config).await.is_err());
    assert_eq!(sqlite_state(&target), before);

    let ancestor_link = root.path().join("ancestor-link");
    symlink(&actual, &ancestor_link).expect("ancestor directory symlink");
    pulse_config.database.sqlite_path = Some(ancestor_link.join("pulse.sqlite3"));
    assert!(open_doctor_store(&pulse_config).await.is_err());
    assert_eq!(sqlite_state(&target), before);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn doctor_preserves_distinct_gauge_states_for_configured_account() {
    let root = TempRoot::new("doctor");
    let config = config(
        &root.path().join("missing-credentials"),
        &[
            ("dead", Vendor::AnthropicOauth),
            ("auth", Vendor::AnthropicOauth),
            ("null", Vendor::AnthropicOauth),
            ("stale", Vendor::AnthropicOauth),
            ("unchanged", Vendor::AnthropicOauth),
        ],
    );
    let store = SqliteStore::open(root.path().join("pulse.sqlite3"))
        .await
        .expect("store");
    let configured = &config.accounts[0];
    store
        .upsert_account(configured.account().expect("account"))
        .await
        .expect("seed account");
    store
        .upsert_machine(Machine {
            account_id: account_id(),
            name: machine(),
            first_seen: at(1_786_214_000_000),
            last_seen: at(1_786_214_400_000),
        })
        .await
        .expect("seed machine");
    let profiles = configured.domain_profiles().expect("profiles");
    for profile in &profiles {
        store
            .upsert_profile(profile.clone())
            .await
            .expect("seed profile");
    }
    let now = at(1_786_214_400_000);
    let by_name = profiles
        .iter()
        .map(|profile| (profile.name.as_str(), profile))
        .collect::<BTreeMap<_, _>>();
    store
        .append_usage_snapshot(failed_snapshot(
            by_name["auth"],
            at(now.epoch_millis() - 60_000),
            CollectionOutcome::AuthenticationFailed {
                code: "auth_failed".to_owned(),
            },
        ))
        .await
        .expect("auth snapshot");
    store
        .append_usage_snapshot(failed_snapshot(
            by_name["null"],
            at(now.epoch_millis() - 60_000),
            CollectionOutcome::Unavailable {
                code: "offline".to_owned(),
            },
        ))
        .await
        .expect("null snapshot");
    store
        .append_usage_snapshot(success_snapshot(
            by_name["stale"],
            at(now.epoch_millis() - 1_800_001),
            20.0,
        ))
        .await
        .expect("stale snapshot");
    store
        .append_usage_snapshot(success_snapshot(
            by_name["unchanged"],
            at(now.epoch_millis() - 1_860_000),
            25.0,
        ))
        .await
        .expect("previous unchanged snapshot");
    store
        .append_usage_snapshot(success_snapshot(
            by_name["unchanged"],
            at(now.epoch_millis() - 60_000),
            25.0,
        ))
        .await
        .expect("latest unchanged snapshot");

    let result = doctor(
        &config,
        &store,
        DoctorRequest {
            account_id: account_id(),
            machine: machine(),
            now,
            paths: local_paths(root.path()),
        },
    )
    .await
    .expect("doctor");

    assert!(result.integrity_ok);
    assert!(result.schema_version > 0);
    assert_eq!(result.profiles.len(), 5);
    let states = result
        .profiles
        .iter()
        .map(|profile| (profile.profile.as_str(), profile.gauge))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(states["dead"], GaugeHealth::DeadNoObservation);
    assert_eq!(states["auth"], GaugeHealth::AuthenticationFailed);
    assert_eq!(states["null"], GaugeHealth::NullSignal);
    assert_eq!(states["stale"], GaugeHealth::Stale);
    assert_eq!(states["unchanged"], GaugeHealth::AuthenticatedUnchanged);
    assert!(result.profiles.iter().all(|profile| profile.persisted));
    assert!(
        result
            .profiles
            .iter()
            .all(|profile| profile.configuration_matches)
    );
}

#[derive(Default)]
struct FakeCollectors {
    usage_calls: AtomicUsize,
    context_calls: AtomicUsize,
    token_calls: AtomicUsize,
    gemini_calls: AtomicUsize,
    seen_profiles: Mutex<Vec<Vec<ProfileName>>>,
}

impl FakeCollectors {
    fn observe(&self, profiles: &[Profile]) {
        self.seen_profiles.lock().expect("seen profiles").push(
            profiles
                .iter()
                .map(|profile| profile.name.clone())
                .collect(),
        );
        assert!(
            profiles
                .iter()
                .all(|profile| profile.origin == ProfileOrigin::Local)
        );
    }
}

impl PulseCollectors for FakeCollectors {
    fn usage(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<UsageSnapshot> {
        self.usage_calls.fetch_add(1, Ordering::SeqCst);
        self.observe(&profiles);
        let profile = profiles[0].clone();
        Box::pin(
            async move { Collected::new(vec![success_snapshot(&profile, collected_at, 12.5)], 0) },
        )
    }

    fn context(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<ContextSession> {
        self.context_calls.fetch_add(1, Ordering::SeqCst);
        self.observe(&profiles);
        let profile = profiles[0].clone();
        Box::pin(async move {
            Collected::new(
                vec![ContextSession {
                    account_id: profile.account_id,
                    profile: profile.name,
                    machine: machine(),
                    session_id: SessionId::new("session-1").expect("session"),
                    model: Some("claude-test".to_owned()),
                    settings: AgentSettings::default(),
                    context_tokens: Some(25),
                    context_percent: Some(Percent::new(25.0).expect("percent")),
                    effective_limit: Some(100),
                    last_active_at: collected_at,
                    last_reset_at: None,
                    collected_at,
                }],
                0,
            )
        })
    }

    fn tokens(
        &self,
        profiles: Vec<Profile>,
        _request: TokenCollectionRequest,
    ) -> CollectionFuture<TokenGrain> {
        self.token_calls.fetch_add(1, Ordering::SeqCst);
        self.observe(&profiles);
        let profile = profiles[0].clone();
        Box::pin(async move { Collected::new(vec![token_grain(&profile)], 0) })
    }

    fn gemini(
        &self,
        profiles: Vec<Profile>,
        _collected_at: Instant,
    ) -> CollectionFuture<GeminiQuota> {
        self.gemini_calls.fetch_add(1, Ordering::SeqCst);
        self.observe(&profiles);
        Box::pin(async { Ok(Collected::empty()) })
    }

    fn completion_push(&self, _completed_at: Instant) -> CompletionFuture {
        Box::pin(async { Ok(()) })
    }
}

fn token_grain(profile: &Profile) -> TokenGrain {
    let settings = AgentSettings::default();
    TokenGrain {
        account_id: profile.account_id,
        profile: profile.name.clone(),
        machine: machine(),
        session_id: SessionId::new("session-1").expect("session"),
        model: "claude-test".to_owned(),
        settings_hash: settings.sha256().expect("hash"),
        settings,
        day: "2026-08-08".to_owned(),
        tokens_in: 10,
        tokens_out: 5,
        cache_write_5m: 0,
        cache_write_1h: 0,
        cache_read: 0,
        source: TokenSource::Local,
    }
}

#[derive(Default)]
struct FakeFullHistory {
    calls: AtomicUsize,
}

impl FullHistoryCollector for FakeFullHistory {
    fn source_generation(
        &self,
        _profile: Profile,
        _machine: MachineName,
    ) -> atmux::pulse::ops::FullHistoryGenerationFuture {
        Box::pin(async { TokenSourceGeneration::new("0".repeat(64)) })
    }

    fn collect(
        &self,
        profile: Profile,
        _machine: MachineName,
        _after: Option<atmux::pulse::token::TokenTallyCursor>,
        max_rows: usize,
    ) -> FullHistoryFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { FullHistoryRows::new(vec![token_grain(&profile)], max_rows) })
    }
}

struct CancellingPagedFullHistory {
    calls: Mutex<Vec<(ProfileName, Option<TokenTallyCursor>)>>,
    cancellation: watch::Sender<bool>,
}

#[derive(Default)]
struct ChangingGenerationFullHistory {
    calls: AtomicUsize,
}

impl FullHistoryCollector for ChangingGenerationFullHistory {
    fn source_generation(
        &self,
        _profile: Profile,
        _machine: MachineName,
    ) -> atmux::pulse::ops::FullHistoryGenerationFuture {
        Box::pin(async { TokenSourceGeneration::new(format!("{:064x}", 1)) })
    }

    fn collect(
        &self,
        profile: Profile,
        _machine: MachineName,
        after: Option<TokenTallyCursor>,
        max_rows: usize,
    ) -> FullHistoryFuture {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let mut row = token_grain(&profile);
            row.session_id = SessionId::new(if after.is_some() {
                "session-02"
            } else {
                "session-01"
            })
            .expect("session");
            let generation = format!("{:064x}", call.saturating_add(1));
            FullHistoryRows::page(
                vec![row],
                max_rows,
                false,
                TokenSourceGeneration::new(generation).expect("source generation"),
            )
        })
    }
}

impl FullHistoryCollector for CancellingPagedFullHistory {
    fn source_generation(
        &self,
        _profile: Profile,
        _machine: MachineName,
    ) -> atmux::pulse::ops::FullHistoryGenerationFuture {
        Box::pin(async { TokenSourceGeneration::new("c".repeat(64)) })
    }

    fn collect(
        &self,
        profile: Profile,
        _machine: MachineName,
        after: Option<TokenTallyCursor>,
        max_rows: usize,
    ) -> FullHistoryFuture {
        let call = {
            let mut calls = self.calls.lock().expect("backfill calls");
            calls.push((profile.name.clone(), after.clone()));
            calls.len()
        };
        let cancellation = self.cancellation.clone();
        Box::pin(async move {
            if call == 5 {
                cancellation.send(true).expect("cancel backfill");
                std::future::pending::<()>().await;
            }
            let rows = (1..=3)
                .map(|index| {
                    let mut row = token_grain(&profile);
                    row.session_id =
                        SessionId::new(format!("session-{index:02}")).expect("session");
                    row.tokens_in = index;
                    row
                })
                .collect::<Vec<_>>();
            let index = after.map_or(0, |cursor| {
                rows.iter()
                    .position(|row| TokenTallyCursor::from_grain(row) == cursor)
                    .expect("known cursor")
                    .saturating_add(1)
            });
            let row = rows.get(index).cloned().expect("remaining backfill row");
            FullHistoryRows::page(
                vec![row],
                max_rows,
                index + 1 == rows.len(),
                TokenSourceGeneration::new("c".repeat(64)).expect("source generation"),
            )
        })
    }
}

#[tokio::test]
async fn push_once_backfill_is_bounded_account_scoped_and_does_not_schedule() {
    let root = TempRoot::new("backfill");
    let config = config(
        &root.path().join("profile"),
        &[("configured", Vendor::AnthropicOauth)],
    );
    let concrete_store = Arc::new(
        SqliteStore::open(root.path().join("pulse.sqlite3"))
            .await
            .expect("store"),
    );
    let store: Arc<dyn Store> = concrete_store.clone();
    let collectors = Arc::new(FakeCollectors::default());
    let backfill = Arc::new(FakeFullHistory::default());
    let sink: Arc<dyn PulseSink> = Arc::new(StoreSink::new(Arc::clone(&store)));
    let (_cancel, mut cancellation) = watch::channel(false);
    let now = at(1_786_214_400_000);

    let result = push_once_with(
        &config,
        Arc::clone(&store),
        collectors.clone(),
        sink,
        backfill.clone(),
        None,
        PushOnceRequest {
            account_id: account_id(),
            machine: machine(),
            started_at: now,
            backfill: true,
            restart_backfill: false,
            paths: local_paths(root.path()),
        },
        &mut cancellation,
    )
    .await
    .expect("push once");

    assert!(!result.cancelled);
    assert_eq!(result.report, PushReportResult::NotConfigured);
    assert_eq!(result.collections.usage.expect("usage").succeeded, 1);
    assert_eq!(result.collections.context.expect("context").succeeded, 1);
    assert_eq!(result.collections.tokens.expect("tokens").succeeded, 1);
    assert_eq!(result.collections.gemini.expect("gemini").attempted, 0);
    assert_eq!(collectors.usage_calls.load(Ordering::SeqCst), 1);
    assert_eq!(collectors.context_calls.load(Ordering::SeqCst), 1);
    assert_eq!(collectors.token_calls.load(Ordering::SeqCst), 0);
    assert_eq!(collectors.gemini_calls.load(Ordering::SeqCst), 1);
    assert_eq!(backfill.calls.load(Ordering::SeqCst), 1);
    assert_eq!(collectors.seen_profiles.lock().expect("seen").len(), 3);

    let profile = ProfileName::new("configured").expect("profile");
    assert_eq!(
        concrete_store
            .usage_history(account_id(), profile.clone(), Some(now), 10)
            .await
            .expect("usage")
            .len(),
        1
    );
    assert_eq!(
        concrete_store
            .list_context_sessions(account_id(), Some(profile.clone()))
            .await
            .expect("contexts")
            .len(),
        1
    );
    assert_eq!(
        concrete_store
            .list_token_grains(account_id(), Some(profile), None, 10)
            .await
            .expect("tokens")
            .len(),
        1
    );
}

#[tokio::test]
async fn cancelled_backfill_resumes_after_the_committed_cursor_without_duplicates() {
    let root = TempRoot::new("backfill-resume");
    let config = config(
        &root.path().join("profile"),
        &[
            ("configured-a", Vendor::AnthropicOauth),
            ("configured-b", Vendor::AnthropicOauth),
        ],
    );
    let concrete_store = Arc::new(
        SqliteStore::open(root.path().join("pulse.sqlite3"))
            .await
            .expect("store"),
    );
    let store: Arc<dyn Store> = concrete_store.clone();
    let collectors = Arc::new(FakeCollectors::default());
    let sink: Arc<dyn PulseSink> = Arc::new(StoreSink::new(Arc::clone(&store)));
    let (cancel, mut cancellation) = watch::channel(false);
    let backfill = Arc::new(CancellingPagedFullHistory {
        calls: Mutex::new(Vec::new()),
        cancellation: cancel,
    });
    let now = at(1_786_214_400_000);

    let interrupted = push_once_with(
        &config,
        Arc::clone(&store),
        collectors.clone(),
        Arc::clone(&sink),
        backfill.clone(),
        None,
        PushOnceRequest {
            account_id: account_id(),
            machine: machine(),
            started_at: now,
            backfill: true,
            restart_backfill: false,
            paths: local_paths(root.path()),
        },
        &mut cancellation,
    )
    .await
    .expect("cancelled backfill result");
    assert!(interrupted.cancelled);
    assert_eq!(collectors.gemini_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        concrete_store
            .list_token_grains(account_id(), None, None, 10,)
            .await
            .expect("committed first page")
            .len(),
        4
    );

    let (_second_cancel, mut second_cancellation) = watch::channel(false);
    let resumed = push_once_with(
        &config,
        store,
        collectors,
        sink,
        backfill.clone(),
        None,
        PushOnceRequest {
            account_id: account_id(),
            machine: machine(),
            started_at: now,
            backfill: true,
            restart_backfill: false,
            paths: local_paths(root.path()),
        },
        &mut second_cancellation,
    )
    .await
    .expect("resumed backfill");
    assert!(!resumed.cancelled);
    assert!(!resumed.collections.backfill_truncated);
    assert_eq!(resumed.collections.tokens.expect("tokens").succeeded, 2);
    assert_eq!(
        concrete_store
            .list_token_grains(account_id(), None, None, 10,)
            .await
            .expect("completed backfill")
            .len(),
        6
    );
    let calls = backfill.calls.lock().expect("backfill calls");
    assert_eq!(calls.len(), 7);
    assert_eq!(
        calls
            .iter()
            .filter(|(profile, _)| profile.as_str() == "configured-a")
            .count(),
        3,
        "the completed first profile must not be recollected on resume"
    );
    assert!(calls[0].1.is_none());
    assert!(calls[1].1.is_some());
    assert!(calls[2].1 > calls[1].1);
    assert!(calls[3].1.is_none());
    assert_eq!(calls[4], calls[5]);
    assert!(calls[6].1 > calls[5].1);
}

#[tokio::test]
async fn continuously_changing_backfill_source_fails_bounded_with_safe_cursor() {
    let root = TempRoot::new("backfill-changing-source");
    let config = config(
        &root.path().join("profile"),
        &[("configured", Vendor::AnthropicOauth)],
    );
    let concrete_store = Arc::new(
        SqliteStore::open(root.path().join("pulse.sqlite3"))
            .await
            .expect("store"),
    );
    let store: Arc<dyn Store> = concrete_store.clone();
    let collectors = Arc::new(FakeCollectors::default());
    let sink: Arc<dyn PulseSink> = Arc::new(StoreSink::new(Arc::clone(&store)));
    let backfill = Arc::new(ChangingGenerationFullHistory::default());
    let (_cancel, mut cancellation) = watch::channel(false);

    let error = push_once_with(
        &config,
        Arc::clone(&store),
        collectors.clone(),
        sink,
        backfill.clone(),
        None,
        PushOnceRequest {
            account_id: account_id(),
            machine: machine(),
            started_at: at(1_786_214_400_000),
            backfill: true,
            restart_backfill: false,
            paths: local_paths(root.path()),
        },
        &mut cancellation,
    )
    .await
    .expect_err("continuously changing source must fail");
    assert_eq!(error.kind(), atmux::pulse::PulseErrorKind::Conflict);
    assert_eq!(backfill.calls.load(Ordering::SeqCst), 5);
    assert_eq!(collectors.gemini_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        concrete_store
            .list_token_grains(
                account_id(),
                Some(ProfileName::new("configured").expect("profile")),
                None,
                10,
            )
            .await
            .expect("only committed rows survive")
            .len(),
        1
    );
    let durable = concrete_store
        .begin_token_backfill(
            account_id(),
            ProfileName::new("configured").expect("profile"),
            machine(),
            TokenSourceGeneration::new(format!("{:064x}", 4)).expect("last accepted generation"),
            false,
        )
        .await
        .expect("inspect durable generation");
    assert!(durable.cursor.is_none());
    assert!(!durable.complete);
}

#[derive(Default)]
struct CapturingReporter {
    requests: Mutex<Vec<Vec<u8>>>,
    debug: Mutex<Vec<String>>,
}

impl ReporterTransport for CapturingReporter {
    fn send(&self, request: ReporterRequest) -> ReporterFuture<ReporterResponse> {
        self.debug
            .lock()
            .expect("debug")
            .push(format!("{request:?}"));
        self.requests
            .lock()
            .expect("requests")
            .push(request.body().to_vec());
        Box::pin(async {
            Ok(ReporterResponse {
                status: 204,
                retry_after: None,
            })
        })
    }
}

#[tokio::test]
async fn push_reports_only_configured_profiles_and_redacts_external_secret() {
    let root = TempRoot::new("report");
    let token_path = root.path().join("report-token");
    let token_canary = "report-secret-canary";
    fs::write(&token_path, token_canary).expect("token file");
    let mut config = config(
        &root.path().join("profile"),
        &[("configured", Vendor::AnthropicOauth)],
    );
    config.report_to = Some("http://127.0.0.1:7345/api/v1/pulse/ingest".to_owned());
    config.report_token_file = Some(token_path.clone());
    let concrete_store = Arc::new(
        SqliteStore::open(root.path().join("pulse.sqlite3"))
            .await
            .expect("store"),
    );
    concrete_store
        .upsert_account(config.accounts[0].account().expect("account"))
        .await
        .expect("seed account");
    concrete_store
        .upsert_profile(Profile {
            account_id: account_id(),
            name: ProfileName::new("stale-local").expect("profile"),
            vendor: Vendor::AnthropicOauth,
            config_dir: Some(root.path().join("stale")),
            poll_interval_minutes: 15,
            monthly_budget_usd: None,
            api_key_env: None,
            api_key_file: None,
            refresh: RefreshPolicy::InMemory,
            hidden: false,
            origin: ProfileOrigin::Local,
        })
        .await
        .expect("seed stale local profile");
    let store: Arc<dyn Store> = concrete_store;
    let collectors = Arc::new(FakeCollectors::default());
    let transport = Arc::new(CapturingReporter::default());
    let reporter = Arc::new(
        PulseReporter::new(
            config.report_to.clone().expect("endpoint"),
            SecretRef::File { path: token_path },
            transport.clone(),
            ReporterBackoff {
                max_attempts: 1,
                jitter_percent: 0,
                ..ReporterBackoff::default()
            },
        )
        .expect("reporter"),
    );
    let sink: Arc<dyn PulseSink> = Arc::new(StoreSink::new(Arc::clone(&store)));
    let (_cancel, mut cancellation) = watch::channel(false);

    let result = push_once_with(
        &config,
        store,
        collectors,
        sink,
        Arc::new(FakeFullHistory::default()),
        Some(reporter),
        PushOnceRequest {
            account_id: account_id(),
            machine: machine(),
            started_at: at(1_786_214_400_000),
            backfill: false,
            restart_backfill: false,
            paths: local_paths(root.path()),
        },
        &mut cancellation,
    )
    .await
    .expect("push once");

    assert!(matches!(result.report, PushReportResult::Sent { .. }));
    let bodies = transport.requests.lock().expect("requests");
    assert!(!bodies.is_empty());
    let reported_profiles = bodies
        .iter()
        .map(|body| serde_json::from_slice::<PushEnvelope>(body).expect("envelope"))
        .flat_map(|envelope| envelope.batch.profiles)
        .map(|profile| profile.name)
        .collect::<Vec<_>>();
    assert_eq!(
        reported_profiles,
        vec![ProfileName::new("configured").expect("profile")]
    );
    assert!(
        transport
            .debug
            .lock()
            .expect("debug")
            .iter()
            .all(|debug| debug.contains("[redacted]") && !debug.contains(token_canary))
    );
}

#[tokio::test]
async fn cancellation_before_start_performs_no_account_write_or_collection() {
    let root = TempRoot::new("cancel");
    let config = config(
        &root.path().join("profile"),
        &[("configured", Vendor::AnthropicOauth)],
    );
    let concrete_store = Arc::new(
        SqliteStore::open(root.path().join("pulse.sqlite3"))
            .await
            .expect("store"),
    );
    let store: Arc<dyn Store> = concrete_store.clone();
    let collectors = Arc::new(FakeCollectors::default());
    let sink: Arc<dyn PulseSink> = Arc::new(StoreSink::new(Arc::clone(&store)));
    let (_cancel, mut cancellation) = watch::channel(true);

    let result = push_once_with(
        &config,
        store,
        collectors.clone(),
        sink,
        Arc::new(FakeFullHistory::default()),
        None,
        PushOnceRequest {
            account_id: account_id(),
            machine: machine(),
            started_at: at(1_786_214_400_000),
            backfill: true,
            restart_backfill: false,
            paths: local_paths(root.path()),
        },
        &mut cancellation,
    )
    .await
    .expect("cancelled result");

    assert!(result.cancelled);
    assert!(result.collections.usage.is_none());
    assert_eq!(collectors.usage_calls.load(Ordering::SeqCst), 0);
    assert!(
        concrete_store
            .get_account(account_id())
            .await
            .expect("get account")
            .is_none()
    );
}

#[tokio::test]
async fn cancellation_during_blocking_prepare_stops_later_phases() {
    let root = TempRoot::new("cancel-prepare");
    let path = root.path().join("pulse.sqlite3");
    let config = config(
        &root.path().join("profile"),
        &[("configured", Vendor::AnthropicOauth)],
    );
    let concrete_store = Arc::new(SqliteStore::open(&path).await.expect("store"));
    let store: Arc<dyn Store> = concrete_store.clone();
    let collectors = Arc::new(FakeCollectors::default());
    let sink: Arc<dyn PulseSink> = Arc::new(StoreSink::new(Arc::clone(&store)));
    let blocker = rusqlite::Connection::open(&path).expect("blocking connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold SQLite writer lock");
    let paths = local_paths(root.path());
    let (cancel, mut cancellation) = watch::channel(false);
    let task_collectors = Arc::clone(&collectors);
    let task = tokio::spawn(async move {
        push_once_with(
            &config,
            store,
            task_collectors,
            sink,
            Arc::new(FakeFullHistory::default()),
            None,
            PushOnceRequest {
                account_id: account_id(),
                machine: machine(),
                started_at: at(1_786_214_400_000),
                backfill: false,
                restart_backfill: false,
                paths,
            },
            &mut cancellation,
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.send(true).expect("request cancellation");
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cancellation must not wait for the blocking SQLite operation")
        .expect("join push task")
        .expect("cancelled result");
    assert!(result.cancelled);
    assert!(result.collections.usage.is_none());
    assert_eq!(collectors.usage_calls.load(Ordering::SeqCst), 0);

    blocker
        .execute_batch("ROLLBACK")
        .expect("release writer lock");
    assert!(
        concrete_store
            .list_profiles(account_id())
            .await
            .expect("profiles after cancellation")
            .is_empty(),
        "a non-cancellable in-flight write may finish, but later profile writes must not start"
    );
    assert!(
        concrete_store
            .list_machines(account_id())
            .await
            .expect("machines after cancellation")
            .is_empty(),
        "machine preparation must not start after cancellation"
    );
}

#[tokio::test]
async fn stored_account_identity_conflict_fails_before_provider_work() {
    let root = TempRoot::new("identity-conflict");
    let config = config(
        &root.path().join("profile"),
        &[("configured", Vendor::AnthropicOauth)],
    );
    let concrete_store = Arc::new(
        SqliteStore::open(root.path().join("pulse.sqlite3"))
            .await
            .expect("store"),
    );
    let mut conflicting = config.accounts[0].account().expect("account");
    conflicting.identity = "different-operator@example.test".to_owned();
    concrete_store
        .upsert_account(conflicting)
        .await
        .expect("conflicting account");
    let store: Arc<dyn Store> = concrete_store;
    let collectors = Arc::new(FakeCollectors::default());
    let sink: Arc<dyn PulseSink> = Arc::new(StoreSink::new(Arc::clone(&store)));
    let (_cancel, mut cancellation) = watch::channel(false);

    let error = push_once_with(
        &config,
        store,
        collectors.clone(),
        sink,
        Arc::new(FakeFullHistory::default()),
        None,
        PushOnceRequest {
            account_id: account_id(),
            machine: machine(),
            started_at: at(1_786_214_400_000),
            backfill: false,
            restart_backfill: false,
            paths: local_paths(root.path()),
        },
        &mut cancellation,
    )
    .await
    .expect_err("identity conflict");

    assert_eq!(error.kind(), atmux::pulse::PulseErrorKind::Conflict);
    assert!(!error.message().contains("different-operator"));
    assert_eq!(collectors.usage_calls.load(Ordering::SeqCst), 0);
}
