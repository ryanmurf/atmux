#![cfg(feature = "pulse")]

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use atmux::pulse::{
    Account, AccountId, AgentSettings, CollectionOutcome, ContextSession, Fraction, GeminiQuota,
    Instant, Machine, MachineName, Percent, Profile, ProfileName, ProfileOrigin, PulseError,
    PulseErrorKind, QuotaWindow, QuotaWindowKind, RefreshPolicy, SessionId, TokenGrain,
    TokenSource, UsageSnapshot, Vendor,
    scheduler::{ClockFuture, ForcePollTarget, JobRunner, PulseJob, SchedulerClock},
    service::{
        Collected, CollectionFuture, CompletionFuture, PersistingJobRunner, ProfileFuture,
        ProfileSource, PulseCollectors, PulseService, PulseSink, SinkFuture, StoreSink,
        TokenCollectionRequest, TokenObservationScope, start_embedded,
    },
    store::{RetentionResult, SqliteStore, Store},
};
use tokio::sync::Notify;

static NEXT_DATABASE_PATH: AtomicU64 = AtomicU64::new(1);

fn private_test_directory(prefix: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        NEXT_DATABASE_PATH.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).expect("create private Pulse test directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mut permissions = std::fs::metadata(&directory)
            .expect("inspect Pulse test directory")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&directory, permissions).expect("protect Pulse test directory");
    }
    directory
}

struct FakeClock {
    now: Arc<AtomicU64>,
    wall: Instant,
    changed: Arc<Notify>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            now: Arc::new(AtomicU64::new(0)),
            wall: Instant::from_iso8601("2026-08-08T18:40:00Z").expect("wall time"),
            changed: Arc::new(Notify::new()),
        }
    }
}

impl SchedulerClock for FakeClock {
    fn monotonic_millis(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    fn wall_now(&self) -> Instant {
        self.wall
    }

    fn sleep_until(&self, deadline_millis: u64) -> ClockFuture {
        let now = Arc::clone(&self.now);
        let changed = Arc::clone(&self.changed);
        Box::pin(async move {
            loop {
                let notified = changed.notified();
                if now.load(Ordering::SeqCst) >= deadline_millis {
                    return;
                }
                notified.await;
            }
        })
    }
}

struct FakeProfiles {
    profile: Profile,
}

struct FakeProfileList {
    profiles: Vec<Profile>,
}

impl ProfileSource for FakeProfileList {
    fn profiles(&self) -> ProfileFuture {
        let profiles = self.profiles.clone();
        Box::pin(async move { Ok(profiles) })
    }
}

impl ProfileSource for FakeProfiles {
    fn profiles(&self) -> ProfileFuture {
        let profile = self.profile.clone();
        Box::pin(async move { Ok(vec![profile]) })
    }
}

#[derive(Default)]
struct FakeCollectors {
    usage: AtomicUsize,
    context: AtomicUsize,
    tokens: AtomicUsize,
    gemini: AtomicUsize,
    completion: AtomicUsize,
    token_lookback: AtomicUsize,
    usage_failures: AtomicUsize,
    fail_usage: AtomicBool,
    usage_account: AtomicUsize,
    context_account: AtomicUsize,
    tokens_account: AtomicUsize,
    gemini_account: AtomicUsize,
    usage_profile_count: AtomicUsize,
    context_profile_count: AtomicUsize,
    tokens_profile_count: AtomicUsize,
    gemini_profile_count: AtomicUsize,
    observe_tokens: AtomicBool,
    pause_tokens: AtomicBool,
    token_entered: Arc<Notify>,
    release_tokens: Arc<Notify>,
}

impl PulseCollectors for FakeCollectors {
    fn token_observation_scopes(&self, profiles: &[Profile]) -> Vec<TokenObservationScope> {
        if !self.observe_tokens.load(Ordering::SeqCst) {
            return Vec::new();
        }
        profiles
            .iter()
            .map(|profile| TokenObservationScope {
                account_id: profile.account_id,
                profile: profile.name.clone(),
                machine: MachineName::new("fixture-machine").expect("machine"),
            })
            .collect()
    }

    fn usage(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<UsageSnapshot> {
        self.usage.fetch_add(1, Ordering::SeqCst);
        let failures = self.usage_failures.load(Ordering::SeqCst);
        let fail = self.fail_usage.load(Ordering::SeqCst);
        self.usage_profile_count
            .store(profiles.len(), Ordering::SeqCst);
        self.usage_account.store(
            usize::try_from(profiles.first().expect("profile").account_id.get()).unwrap(),
            Ordering::SeqCst,
        );
        Box::pin(async move {
            if fail {
                return Err(PulseError::new(
                    PulseErrorKind::Upstream,
                    "collector-secret-canary",
                ));
            }
            let profile = profiles.first().expect("profile");
            Collected::new(vec![usage_snapshot(profile, collected_at)], failures)
        })
    }

    fn context(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<ContextSession> {
        self.context.fetch_add(1, Ordering::SeqCst);
        self.context_profile_count
            .store(profiles.len(), Ordering::SeqCst);
        self.context_account.store(
            usize::try_from(profiles.first().expect("profile").account_id.get()).unwrap(),
            Ordering::SeqCst,
        );
        Box::pin(async move {
            Collected::new(
                vec![context_session(
                    profiles.first().expect("profile"),
                    collected_at,
                )],
                0,
            )
        })
    }

    fn tokens(
        &self,
        profiles: Vec<Profile>,
        request: TokenCollectionRequest,
    ) -> CollectionFuture<TokenGrain> {
        self.tokens.fetch_add(1, Ordering::SeqCst);
        self.tokens_profile_count
            .store(profiles.len(), Ordering::SeqCst);
        self.token_lookback
            .store(usize::from(request.lookback_days), Ordering::SeqCst);
        self.tokens_account.store(
            usize::try_from(profiles.first().expect("profile").account_id.get()).unwrap(),
            Ordering::SeqCst,
        );
        let pause = self.pause_tokens.load(Ordering::SeqCst);
        let entered = Arc::clone(&self.token_entered);
        let release = Arc::clone(&self.release_tokens);
        Box::pin(async move {
            if pause {
                entered.notify_one();
                release.notified().await;
            }
            Collected::new(vec![token_grain(profiles.first().expect("profile"))], 0)
        })
    }

    fn gemini(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<GeminiQuota> {
        self.gemini.fetch_add(1, Ordering::SeqCst);
        self.gemini_profile_count
            .store(profiles.len(), Ordering::SeqCst);
        let account_id = profiles.first().expect("profile").account_id;
        self.gemini_account
            .store(usize::try_from(account_id.get()).unwrap(), Ordering::SeqCst);
        Box::pin(async move {
            Collected::new(
                vec![GeminiQuota {
                    account_id,
                    model_id: "gemini-fixture".to_owned(),
                    remaining_fraction: Fraction::new(0.5).expect("fraction"),
                    remaining_amount: None,
                    resets_at: None,
                    collected_at,
                }],
                0,
            )
        })
    }

    fn completion_push(&self, _completed_at: Instant) -> CompletionFuture {
        self.completion.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct FakeSink {
    usage: AtomicUsize,
    context: AtomicUsize,
    tokens: AtomicUsize,
    gemini: AtomicUsize,
    retention: AtomicUsize,
    fail_next_usage: AtomicBool,
}

impl PulseSink for FakeSink {
    fn usage(&self, _snapshot: UsageSnapshot) -> SinkFuture<()> {
        self.usage.fetch_add(1, Ordering::SeqCst);
        let fail = self.fail_next_usage.swap(false, Ordering::SeqCst);
        Box::pin(async move {
            if fail {
                Err(PulseError::new(
                    PulseErrorKind::Storage,
                    "fake sink failure",
                ))
            } else {
                Ok(())
            }
        })
    }

    fn context(&self, _session: ContextSession) -> SinkFuture<()> {
        self.context.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn tokens(&self, _grain: TokenGrain) -> SinkFuture<()> {
        self.tokens.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn gemini(&self, _quota: GeminiQuota) -> SinkFuture<()> {
        self.gemini.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn retention(
        &self,
        _now: Instant,
        _settings: atmux::pulse::PulseRetentionConfig,
    ) -> SinkFuture<RetentionResult> {
        self.retention.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(RetentionResult::default()) })
    }
}

#[tokio::test]
async fn embedded_service_runs_each_typed_job_once_and_shuts_down() {
    let config = atmux::pulse::PulseConfig {
        collect: true,
        ..atmux::pulse::PulseConfig::default()
    };
    let profile = profile();
    let collectors = Arc::new(FakeCollectors::default());
    let sink = Arc::new(FakeSink::default());
    let service = PulseService::start_with_clock(
        &config,
        Arc::new(FakeProfiles { profile }),
        collectors.clone(),
        sink.clone(),
        Arc::new(FakeClock::new()),
        7,
    )
    .expect("start service")
    .expect("enabled");

    wait_until(|| {
        sink.usage.load(Ordering::SeqCst) == 1
            && sink.context.load(Ordering::SeqCst) == 1
            && sink.tokens.load(Ordering::SeqCst) == 1
            && sink.gemini.load(Ordering::SeqCst) == 1
            && sink.retention.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(collectors.usage.load(Ordering::SeqCst), 1);
    assert_eq!(collectors.context.load(Ordering::SeqCst), 1);
    assert_eq!(collectors.tokens.load(Ordering::SeqCst), 1);
    assert_eq!(collectors.gemini.load(Ordering::SeqCst), 1);
    assert_eq!(collectors.token_lookback.load(Ordering::SeqCst), 2);

    service.notify_completion().expect("completion trigger");
    wait_until(|| collectors.completion.load(Ordering::SeqCst) == 1).await;
    service.shutdown().await;
}

#[tokio::test]
async fn serve_only_service_runs_retention_without_collecting() {
    let config = atmux::pulse::PulseConfig {
        serve: true,
        receive: true,
        ..atmux::pulse::PulseConfig::default()
    };
    let collectors = Arc::new(FakeCollectors::default());
    let sink = Arc::new(FakeSink::default());
    let service = PulseService::start_with_clock(
        &config,
        Arc::new(FakeProfiles { profile: profile() }),
        collectors.clone(),
        sink.clone(),
        Arc::new(FakeClock::new()),
        9,
    )
    .expect("start retention service")
    .expect("serve/receive retention is enabled");

    wait_until(|| sink.retention.load(Ordering::SeqCst) == 1).await;
    assert_eq!(collectors.usage.load(Ordering::SeqCst), 0);
    assert_eq!(collectors.context.load(Ordering::SeqCst), 0);
    assert_eq!(collectors.tokens.load(Ordering::SeqCst), 0);
    assert_eq!(collectors.gemini.load(Ordering::SeqCst), 0);
    assert!(service.notify_completion().is_err());
    service.shutdown().await;
}

#[tokio::test]
async fn serve_receive_runtime_exposes_receiver_without_enabling_collectors() {
    let directory = private_test_directory("atmux-pulse-receive-runtime");
    let path = directory.join("pulse.sqlite3");
    let config = atmux::pulse::PulseConfig {
        serve: true,
        receive: true,
        database: atmux::pulse::PulseDatabaseConfig {
            sqlite_path: Some(path.clone()),
            postgres_url_env: None,
        },
        ..atmux::pulse::PulseConfig::default()
    };
    let runtime = start_embedded(&config, "receiver-test")
        .await
        .expect("start runtime")
        .expect("serve/receive runtime");
    assert!(runtime.receiver().is_some());
    assert!(runtime.management().is_some());
    assert_eq!(
        runtime
            .management()
            .expect("management")
            .force_poll(ForcePollTarget::account(
                AccountId::new(1).expect("account"),
            ))
            .expect_err("collection stays disabled")
            .kind(),
        PulseErrorKind::Conflict
    );
    runtime.shutdown().await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let _ = std::fs::remove_dir(directory);

    let invalid = atmux::pulse::PulseConfig {
        receive: true,
        ..atmux::pulse::PulseConfig::default()
    };
    assert_eq!(
        invalid
            .validate()
            .expect_err("receive requires serve")
            .kind(),
        PulseErrorKind::Configuration
    );
}

#[tokio::test]
async fn persisting_runner_counts_collector_and_store_failures_without_leaking_errors() {
    let collectors = Arc::new(FakeCollectors::default());
    collectors.usage_failures.store(1, Ordering::SeqCst);
    let sink = Arc::new(FakeSink::default());
    sink.fail_next_usage.store(true, Ordering::SeqCst);
    let runner = PersistingJobRunner::new(
        Arc::new(FakeProfiles { profile: profile() }),
        collectors.clone(),
        sink,
        atmux::pulse::PulseRetentionConfig::default(),
        2,
    );
    let report = runner
        .run(
            PulseJob::Usage,
            Instant::from_iso8601("2026-08-08T18:40:00Z").expect("instant"),
        )
        .await
        .expect("fail-soft report");
    assert_eq!(report.attempted, 2);
    assert_eq!(report.succeeded, 0);
    assert_eq!(report.failed, 2);
    assert!(!format!("{report:?}").contains("canary"));

    collectors.fail_usage.store(true, Ordering::SeqCst);
    let report = runner
        .run(
            PulseJob::Usage,
            Instant::from_iso8601("2026-08-08T18:40:00Z").expect("instant"),
        )
        .await
        .expect("collector error is absorbed");
    assert_eq!(report.failed, 1);
    assert!(!format!("{report:?}").contains("collector-secret-canary"));
}

#[tokio::test]
async fn force_poll_filters_account_and_profile_before_every_collector_and_persists() {
    let mut account_two = profile();
    account_two.account_id = AccountId::new(2).expect("account two");
    account_two.name = ProfileName::new("fixture-two").expect("profile two");
    let mut account_two_sibling = account_two.clone();
    account_two_sibling.name = ProfileName::new("fixture-two-sibling").expect("sibling profile");
    let collectors = Arc::new(FakeCollectors::default());
    let sink = Arc::new(FakeSink::default());
    let runner = PersistingJobRunner::new(
        Arc::new(FakeProfileList {
            profiles: vec![profile(), account_two_sibling, account_two],
        }),
        collectors.clone(),
        sink.clone(),
        atmux::pulse::PulseRetentionConfig::default(),
        2,
    );
    let report = runner
        .run(
            PulseJob::ForcePoll(ForcePollTarget::profile(
                AccountId::new(2).expect("account two"),
                ProfileName::new("fixture-two").expect("profile two"),
            )),
            Instant::from_iso8601("2026-08-08T18:40:00Z").expect("instant"),
        )
        .await
        .expect("force poll report");
    assert_eq!(report.succeeded, 4);
    assert_eq!(report.failed, 0);
    assert_eq!(collectors.usage_account.load(Ordering::SeqCst), 2);
    assert_eq!(collectors.context_account.load(Ordering::SeqCst), 2);
    assert_eq!(collectors.tokens_account.load(Ordering::SeqCst), 2);
    assert_eq!(collectors.gemini_account.load(Ordering::SeqCst), 2);
    assert_eq!(collectors.usage_profile_count.load(Ordering::SeqCst), 1);
    assert_eq!(collectors.context_profile_count.load(Ordering::SeqCst), 1);
    assert_eq!(collectors.tokens_profile_count.load(Ordering::SeqCst), 1);
    assert_eq!(collectors.gemini_profile_count.load(Ordering::SeqCst), 1);
    assert_eq!(sink.usage.load(Ordering::SeqCst), 1);
    assert_eq!(sink.context.load(Ordering::SeqCst), 1);
    assert_eq!(sink.tokens.load(Ordering::SeqCst), 1);
    assert_eq!(sink.gemini.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn token_jobs_reserve_their_store_revision_before_scanning() {
    let directory = private_test_directory("atmux-pulse-observed-token-job");
    let path = directory.join("pulse.sqlite3");
    let store = Arc::new(SqliteStore::open(&path).await.expect("store"));
    let profile = profile();
    let machine = MachineName::new("fixture-machine").expect("machine");
    store
        .upsert_account(Account {
            id: profile.account_id,
            identity: "fixture@example.test".to_owned(),
            display_name: None,
        })
        .await
        .expect("account");
    store
        .upsert_machine(Machine {
            account_id: profile.account_id,
            name: machine.clone(),
            first_seen: Instant::from_epoch_millis(1).expect("first seen"),
            last_seen: Instant::from_epoch_millis(2).expect("last seen"),
        })
        .await
        .expect("machine");
    store
        .upsert_profile(profile.clone())
        .await
        .expect("profile");

    let stale = store
        .begin_token_observation(profile.account_id, profile.name.clone(), machine)
        .await
        .expect("stale observation");
    let collectors = Arc::new(FakeCollectors::default());
    collectors.observe_tokens.store(true, Ordering::SeqCst);
    collectors.pause_tokens.store(true, Ordering::SeqCst);
    let entered = collectors.token_entered.notified();
    let runner = PersistingJobRunner::new(
        Arc::new(FakeProfiles {
            profile: profile.clone(),
        }),
        collectors.clone(),
        Arc::new(StoreSink::new(store.clone())),
        atmux::pulse::PulseRetentionConfig::default(),
        2,
    );
    let task = tokio::spawn(async move {
        runner
            .run(
                PulseJob::Tokens,
                Instant::from_iso8601("2026-08-08T18:40:00Z").expect("instant"),
            )
            .await
            .expect("token job")
    });
    entered.await;

    let mut stale_row = token_grain(&profile);
    stale_row.tokens_in = 1;
    assert_eq!(
        store
            .upsert_observed_token_grain(stale, stale_row)
            .await
            .expect_err("runner reserved a newer observation")
            .kind(),
        PulseErrorKind::Conflict
    );
    collectors.release_tokens.notify_one();
    let report = task.await.expect("join token job");
    assert_eq!(report.succeeded, 1);
    assert_eq!(report.failed, 0);
    let stored = store
        .list_token_grains(profile.account_id, Some(profile.name), None, 10)
        .await
        .expect("stored tokens");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].tokens_in, 10);

    drop(store);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let _ = std::fs::remove_dir(directory);
}

#[test]
fn service_is_inert_by_default() {
    let result = PulseService::start_with_clock(
        &atmux::pulse::PulseConfig::default(),
        Arc::new(FakeProfiles { profile: profile() }),
        Arc::new(FakeCollectors::default()),
        Arc::new(FakeSink::default()),
        Arc::new(FakeClock::new()),
        0,
    )
    .expect("safe default");
    assert!(result.is_none());
}

fn profile() -> Profile {
    Profile {
        account_id: AccountId::new(1).expect("account"),
        name: ProfileName::new("fixture").expect("profile"),
        vendor: Vendor::AnthropicOauth,
        origin: ProfileOrigin::Local,
        config_dir: None,
        poll_interval_minutes: 15,
        monthly_budget_usd: None,
        api_key_env: None,
        api_key_file: None,
        refresh: RefreshPolicy::InMemory,
        hidden: false,
    }
}

fn usage_snapshot(profile: &Profile, at: Instant) -> UsageSnapshot {
    UsageSnapshot {
        account_id: profile.account_id,
        profile: profile.name.clone(),
        machine: MachineName::new("fixture-machine").expect("machine"),
        vendor: Vendor::AnthropicOauth,
        windows: vec![QuotaWindow {
            kind: QuotaWindowKind::FiveHour,
            used_percent: Percent::new(25.0).expect("percent"),
            resets_at: Instant::from_iso8601("2026-08-09T00:00:00Z").expect("reset"),
        }],
        outcome: CollectionOutcome::Success,
        polled_at: at,
        reporter_version: Some("test".to_owned()),
    }
}

fn context_session(profile: &Profile, at: Instant) -> ContextSession {
    ContextSession {
        account_id: profile.account_id,
        profile: profile.name.clone(),
        machine: MachineName::new("fixture-machine").expect("machine"),
        session_id: SessionId::new("fixture-session").expect("session"),
        model: Some("fixture-model".to_owned()),
        settings: AgentSettings::default(),
        context_tokens: Some(50),
        context_percent: Some(Percent::new(25.0).expect("percent")),
        effective_limit: Some(200),
        last_active_at: at,
        last_reset_at: None,
        collected_at: at,
    }
}

fn token_grain(profile: &Profile) -> TokenGrain {
    let settings = AgentSettings::default();
    let settings_hash = settings.sha256().expect("settings hash");
    TokenGrain {
        account_id: profile.account_id,
        profile: profile.name.clone(),
        machine: MachineName::new("fixture-machine").expect("machine"),
        session_id: SessionId::new("fixture-session").expect("session"),
        model: "fixture-model".to_owned(),
        settings,
        settings_hash,
        day: "2026-08-08".to_owned(),
        tokens_in: 10,
        tokens_out: 5,
        cache_write_5m: 0,
        cache_write_1h: 0,
        cache_read: 0,
        source: TokenSource::Local,
    }
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..2_000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition did not become true");
}
