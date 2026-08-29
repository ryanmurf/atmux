//! Embedded Pulse collection service and typed collector/store boundaries.
//!
//! On Midnight this service must run inside the existing Aqua `LaunchAgent` web
//! process so macOS Keychain access inherits the login security session. This
//! module never starts, restarts, or replaces that process itself.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime},
};

use directories::{ProjectDirs, UserDirs};
use tokio::sync::watch;

use super::{
    AccountId, ContextSession, GeminiQuota, Instant, Machine, MachineName, Profile, ProfileOrigin,
    PulseError, PulseErrorKind, PulseResult, TokenGrain, UsageSnapshot,
    alerts::{
        deliver_triggered_alerts, evaluate_context_alerts, evaluate_usage_alerts, record_due_alerts,
    },
    collect::SecretRef,
    config::{PulseConfig, PulseRetentionConfig},
    delivery::{ControlPlaneAlertSink, PulseNotificationSink, UnavailableAlertSink},
    federation::{
        AtmuxPullTransport, DirectFederationExporter, FederationPullLifecycle, PulseFederation,
        StoreFederationConsumer, StoreFederationSource,
    },
    ingest::IngestReceiver,
    invalidation::PulseInvalidationHub,
    native::NativeCollectors,
    preflight::preflight_profiles,
    reporter::{
        AccountReporterOutcome, HttpReporterTransport, PulseReporter, ReporterBackoff,
        StoreReporterCoordinator,
    },
    reset::{ResetNotificationSink, ResetResumeScheduler, schedule_rate_limit_resume},
    scheduler::{
        ForcePollTarget, JobFuture, JobReport, JobRunner, PulseJob, SchedulerClock,
        SchedulerHandle, SchedulerSettings, SystemClock, spawn_scheduler,
    },
    store::{
        IngestLimits, ResetResumeLimits, RetentionResult, SqliteStore, Store, TokenWriteObservation,
    },
};
use crate::{control::ControlPlane, remote::RemoteMachine};

const MAX_COLLECTION_ITEMS: usize = 10_000;

pub type ProfileFuture = Pin<Box<dyn Future<Output = PulseResult<Vec<Profile>>> + Send + 'static>>;
pub type CollectionFuture<T> =
    Pin<Box<dyn Future<Output = PulseResult<Collected<T>>> + Send + 'static>>;
pub type CompletionFuture = Pin<Box<dyn Future<Output = PulseResult<()>> + Send + 'static>>;
pub type SinkFuture<T> = Pin<Box<dyn Future<Output = PulseResult<T>> + Send + 'static>>;

/// A bounded typed collection. Provider-specific failures are counted without
/// retaining raw bodies, credentials, or error strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collected<T> {
    items: Vec<T>,
    failures: usize,
}

impl<T> Collected<T> {
    /// Creates a bounded collection result.
    ///
    /// # Errors
    ///
    /// Returns a storage error if a collector exceeds the shared item/failure
    /// bound.
    pub fn new(items: Vec<T>, failures: usize) -> PulseResult<Self> {
        if items.len().saturating_add(failures) > MAX_COLLECTION_ITEMS {
            return Err(PulseError::new(
                PulseErrorKind::Storage,
                "Pulse collector exceeded its result bound",
            ));
        }
        Ok(Self { items, failures })
    }

    #[must_use]
    pub const fn empty() -> Self {
        Self {
            items: Vec::new(),
            failures: 0,
        }
    }
}

/// Supplies configured provider profiles without inventing account identity.
pub trait ProfileSource: Send + Sync + 'static {
    fn profiles(&self) -> ProfileFuture;
}

/// Safe production default until profile bootstrap/discovery is wired by its
/// dedicated package. Explicit collection runs do no provider work instead of
/// guessing account 1 or harvesting unrelated CLI homes.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyProfileSource;

impl ProfileSource for EmptyProfileSource {
    fn profiles(&self) -> ProfileFuture {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// Runtime profile source backed by explicit account-scoped store reads.
/// This makes bounded profile setting mutations effective without admitting
/// reported profiles to local credential-bearing collectors.
#[derive(Clone)]
struct StoreProfileSource {
    store: Arc<dyn Store>,
    accounts: Arc<[AccountId]>,
}

impl ProfileSource for StoreProfileSource {
    fn profiles(&self) -> ProfileFuture {
        let store = Arc::clone(&self.store);
        let accounts = Arc::clone(&self.accounts);
        Box::pin(async move {
            let mut profiles = Vec::new();
            for account_id in accounts.iter().copied() {
                profiles.extend(
                    store
                        .list_profiles(account_id)
                        .await?
                        .into_iter()
                        .filter(|profile| profile.origin == ProfileOrigin::Local),
                );
            }
            Ok(profiles)
        })
    }
}

/// Token collection carries the configured bounded recent lookback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenCollectionRequest {
    pub collected_at: Instant,
    pub lookback_days: u16,
}

/// One local token source whose write revision must be reserved before scanning.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TokenObservationScope {
    pub account_id: AccountId,
    pub profile: super::ProfileName,
    pub machine: MachineName,
}

/// Typed adapter boundary for WP3/WP4 collectors.
///
/// Claude, Codex, `DeepSeek`, and Grok normalize into `UsageSnapshot`; Gemini
/// normalizes into `GeminiQuota`. Context and token collectors use their
/// existing domain rows. No provider response or credential can enter this
/// interface.
pub trait PulseCollectors: Send + Sync + 'static {
    /// Returns the exact local token scopes this collector may emit.
    ///
    /// The runner reserves these revisions before invoking `tokens`, closing
    /// stale scan/backfill races. Test and safe-empty collectors default to the
    /// legacy unobserved path because they do not read a mutable token source.
    fn token_observation_scopes(&self, _profiles: &[Profile]) -> Vec<TokenObservationScope> {
        Vec::new()
    }

    fn usage(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<UsageSnapshot>;
    fn context(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<ContextSession>;
    fn tokens(
        &self,
        profiles: Vec<Profile>,
        request: TokenCollectionRequest,
    ) -> CollectionFuture<TokenGrain>;
    fn gemini(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<GeminiQuota>;
    fn force_usage(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<UsageSnapshot> {
        self.usage(profiles, collected_at)
    }
    fn force_context(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<ContextSession> {
        self.context(profiles, collected_at)
    }
    fn force_tokens(
        &self,
        profiles: Vec<Profile>,
        request: TokenCollectionRequest,
    ) -> CollectionFuture<TokenGrain> {
        self.tokens(profiles, request)
    }
    fn force_gemini(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<GeminiQuota> {
        self.gemini(profiles, collected_at)
    }
    fn completion_push(&self, completed_at: Instant) -> CompletionFuture;
}

/// Safe-empty collector set used only while no profile source is configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyCollectors;

impl PulseCollectors for EmptyCollectors {
    fn usage(
        &self,
        _profiles: Vec<Profile>,
        _collected_at: Instant,
    ) -> CollectionFuture<UsageSnapshot> {
        Box::pin(async { Ok(Collected::empty()) })
    }

    fn context(
        &self,
        _profiles: Vec<Profile>,
        _collected_at: Instant,
    ) -> CollectionFuture<ContextSession> {
        Box::pin(async { Ok(Collected::empty()) })
    }

    fn tokens(
        &self,
        _profiles: Vec<Profile>,
        _request: TokenCollectionRequest,
    ) -> CollectionFuture<TokenGrain> {
        Box::pin(async { Ok(Collected::empty()) })
    }

    fn gemini(
        &self,
        _profiles: Vec<Profile>,
        _collected_at: Instant,
    ) -> CollectionFuture<GeminiQuota> {
        Box::pin(async { Ok(Collected::empty()) })
    }

    fn completion_push(&self, _completed_at: Instant) -> CompletionFuture {
        Box::pin(async { Ok(()) })
    }
}

/// Narrow persistence boundary used by the scheduler and fake-store tests.
pub trait PulseSink: Send + Sync + 'static {
    fn usage(&self, snapshot: UsageSnapshot) -> SinkFuture<()>;
    fn context(&self, session: ContextSession) -> SinkFuture<()>;
    fn tokens(&self, grain: TokenGrain) -> SinkFuture<()>;
    fn begin_token_observation(
        &self,
        _scope: TokenObservationScope,
    ) -> SinkFuture<TokenWriteObservation> {
        Box::pin(async {
            Err(PulseError::new(
                PulseErrorKind::Configuration,
                "Pulse sink does not support observed token writes",
            ))
        })
    }
    fn observed_tokens(
        &self,
        _observation: TokenWriteObservation,
        _grain: TokenGrain,
    ) -> SinkFuture<()> {
        Box::pin(async {
            Err(PulseError::new(
                PulseErrorKind::Configuration,
                "Pulse sink does not support observed token writes",
            ))
        })
    }
    fn gemini(&self, quota: GeminiQuota) -> SinkFuture<()>;
    fn retention(
        &self,
        now: Instant,
        settings: PulseRetentionConfig,
    ) -> SinkFuture<RetentionResult>;
}

/// Adapter from the complete Store contract to the scheduler's narrow sink.
pub struct StoreSink {
    store: Arc<dyn Store>,
    notifications: Arc<dyn PulseNotificationSink>,
    invalidations: Option<PulseInvalidationHub>,
    accounts: Arc<[AccountId]>,
}

impl StoreSink {
    #[must_use]
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self::with_notifications(store, Arc::new(UnavailableAlertSink))
    }

    #[must_use]
    pub fn with_notifications(
        store: Arc<dyn Store>,
        notifications: Arc<dyn PulseNotificationSink>,
    ) -> Self {
        Self {
            store,
            notifications,
            invalidations: None,
            accounts: Arc::from([]),
        }
    }

    #[must_use]
    pub fn with_runtime_invalidations(
        store: Arc<dyn Store>,
        notifications: Arc<dyn PulseNotificationSink>,
        invalidations: PulseInvalidationHub,
        accounts: Arc<[AccountId]>,
    ) -> Self {
        Self {
            store,
            notifications,
            invalidations: Some(invalidations),
            accounts,
        }
    }
}

impl PulseSink for StoreSink {
    fn usage(&self, snapshot: UsageSnapshot) -> SinkFuture<()> {
        let store = Arc::clone(&self.store);
        let notifications = Arc::clone(&self.notifications);
        let invalidations = self.invalidations.clone();
        Box::pin(async move {
            store.append_usage_snapshot(snapshot.clone()).await?;
            let downstream = async {
                let subscriptions = store.list_alert_subscriptions(snapshot.account_id).await?;
                let candidates = evaluate_usage_alerts(&snapshot, &subscriptions)?;
                let triggered = record_due_alerts(store.as_ref(), candidates).await?;
                let _ = schedule_rate_limit_resume(
                    store.as_ref(),
                    &snapshot,
                    snapshot.polled_at,
                    ResetResumeLimits::default(),
                )
                .await?;
                let _ = deliver_triggered_alerts(notifications.as_ref(), &triggered).await;
                Ok(())
            }
            .await;
            if let Some(invalidations) = invalidations {
                let _ = invalidations.publish(snapshot.account_id);
            }
            downstream
        })
    }

    fn context(&self, session: ContextSession) -> SinkFuture<()> {
        let store = Arc::clone(&self.store);
        let notifications = Arc::clone(&self.notifications);
        let invalidations = self.invalidations.clone();
        Box::pin(async move {
            store.upsert_context_session(session.clone()).await?;
            let downstream = async {
                let subscriptions = store.list_alert_subscriptions(session.account_id).await?;
                let candidates = evaluate_context_alerts(&session, &subscriptions)?;
                let triggered = record_due_alerts(store.as_ref(), candidates).await?;
                let _ = deliver_triggered_alerts(notifications.as_ref(), &triggered).await;
                Ok(())
            }
            .await;
            if let Some(invalidations) = invalidations {
                let _ = invalidations.publish(session.account_id);
            }
            downstream
        })
    }

    fn tokens(&self, grain: TokenGrain) -> SinkFuture<()> {
        let store = Arc::clone(&self.store);
        let invalidations = self.invalidations.clone();
        Box::pin(async move {
            let account_id = grain.account_id;
            store.upsert_token_grain(grain).await?;
            if let Some(invalidations) = invalidations {
                let _ = invalidations.publish(account_id);
            }
            Ok(())
        })
    }

    fn begin_token_observation(
        &self,
        scope: TokenObservationScope,
    ) -> SinkFuture<TokenWriteObservation> {
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            store
                .begin_token_observation(scope.account_id, scope.profile, scope.machine)
                .await
        })
    }

    fn observed_tokens(
        &self,
        observation: TokenWriteObservation,
        grain: TokenGrain,
    ) -> SinkFuture<()> {
        let store = Arc::clone(&self.store);
        let invalidations = self.invalidations.clone();
        Box::pin(async move {
            let account_id = grain.account_id;
            store
                .upsert_observed_token_grain(observation, grain)
                .await?;
            if let Some(invalidations) = invalidations {
                let _ = invalidations.publish(account_id);
            }
            Ok(())
        })
    }

    fn gemini(&self, quota: GeminiQuota) -> SinkFuture<()> {
        let store = Arc::clone(&self.store);
        let invalidations = self.invalidations.clone();
        Box::pin(async move {
            let account_id = quota.account_id;
            store.upsert_gemini_quota(quota).await?;
            if let Some(invalidations) = invalidations {
                let _ = invalidations.publish(account_id);
            }
            Ok(())
        })
    }

    fn retention(
        &self,
        now: Instant,
        settings: PulseRetentionConfig,
    ) -> SinkFuture<RetentionResult> {
        let store = Arc::clone(&self.store);
        let invalidations = self.invalidations.clone();
        let accounts = Arc::clone(&self.accounts);
        Box::pin(async move {
            let result = store
                .apply_retention(
                    now,
                    settings.context_days,
                    settings.alert_days,
                    settings.hourly_snapshots_after_days,
                    settings.daily_snapshots_after_days,
                )
                .await?;
            if let Some(invalidations) = invalidations {
                for account_id in accounts.iter().copied() {
                    let _ = invalidations.publish(account_id);
                }
            }
            Ok(result)
        })
    }
}

/// Fail-soft collector/persistence implementation used by the scheduler.
pub struct PersistingJobRunner {
    profiles: Arc<dyn ProfileSource>,
    collectors: Arc<dyn PulseCollectors>,
    sink: Arc<dyn PulseSink>,
    retention: PulseRetentionConfig,
    token_lookback_days: u16,
    completion_reporter: Option<CompletionReporter>,
}

#[derive(Clone)]
struct CompletionReporter {
    coordinator: Arc<StoreReporterCoordinator>,
    cancellation: watch::Receiver<bool>,
}

impl PersistingJobRunner {
    #[must_use]
    pub fn new(
        profiles: Arc<dyn ProfileSource>,
        collectors: Arc<dyn PulseCollectors>,
        sink: Arc<dyn PulseSink>,
        retention: PulseRetentionConfig,
        token_lookback_days: u16,
    ) -> Self {
        Self {
            profiles,
            collectors,
            sink,
            retention,
            token_lookback_days,
            completion_reporter: None,
        }
    }

    fn with_completion_reporter(
        mut self,
        coordinator: Arc<StoreReporterCoordinator>,
        cancellation: watch::Receiver<bool>,
    ) -> Self {
        self.completion_reporter = Some(CompletionReporter {
            coordinator,
            cancellation,
        });
        self
    }

    async fn profiles(&self) -> Result<Vec<Profile>, JobReport> {
        self.profiles
            .profiles()
            .await
            .map_err(|_| JobReport::one_failure())
    }

    async fn persist_usage(&self, collected: Collected<UsageSnapshot>) -> JobReport {
        persist_rows(collected, |row| self.sink.usage(row)).await
    }

    async fn persist_context(&self, collected: Collected<ContextSession>) -> JobReport {
        persist_rows(collected, |row| self.sink.context(row)).await
    }

    async fn persist_tokens(&self, collected: Collected<TokenGrain>) -> JobReport {
        persist_rows(collected, |row| self.sink.tokens(row)).await
    }

    async fn collect_and_persist_tokens(
        &self,
        profiles: Vec<Profile>,
        request: TokenCollectionRequest,
        force: bool,
    ) -> JobReport {
        let scopes = self.collectors.token_observation_scopes(&profiles);
        let mut observations = BTreeMap::new();
        for scope in scopes {
            let key = (
                scope.account_id,
                scope.profile.clone(),
                scope.machine.clone(),
            );
            if observations.contains_key(&key) {
                return JobReport::one_failure();
            }
            let Ok(observation) = self.sink.begin_token_observation(scope).await else {
                return JobReport::one_failure();
            };
            observations.insert(key, observation);
        }
        let collected = if force {
            self.collectors.force_tokens(profiles, request).await
        } else {
            self.collectors.tokens(profiles, request).await
        };
        let Ok(collected) = collected else {
            return JobReport::one_failure();
        };
        if observations.is_empty() {
            return self.persist_tokens(collected).await;
        }
        persist_observed_tokens(collected, &observations, self.sink.as_ref()).await
    }

    async fn persist_gemini(&self, collected: Collected<GeminiQuota>) -> JobReport {
        persist_rows(collected, |row| self.sink.gemini(row)).await
    }

    async fn force_poll(&self, target: ForcePollTarget, triggered_at: Instant) -> JobReport {
        let Ok(profiles) = self.profiles().await else {
            return JobReport::one_failure();
        };
        let profiles = profiles
            .into_iter()
            .filter(|profile| {
                profile.account_id == target.account_id
                    && target
                        .profile
                        .as_ref()
                        .is_none_or(|name| &profile.name == name)
            })
            .collect::<Vec<_>>();
        let mut report = match self
            .collectors
            .force_usage(profiles.clone(), triggered_at)
            .await
        {
            Ok(collected) => self.persist_usage(collected).await,
            Err(_) => JobReport::one_failure(),
        };
        report = report.combine(
            match self
                .collectors
                .force_context(profiles.clone(), triggered_at)
                .await
            {
                Ok(collected) => self.persist_context(collected).await,
                Err(_) => JobReport::one_failure(),
            },
        );
        report = report.combine(
            self.collect_and_persist_tokens(
                profiles.clone(),
                TokenCollectionRequest {
                    collected_at: triggered_at,
                    lookback_days: self.token_lookback_days,
                },
                true,
            )
            .await,
        );
        report.combine(
            match self.collectors.force_gemini(profiles, triggered_at).await {
                Ok(collected) => self.persist_gemini(collected).await,
                Err(_) => JobReport::one_failure(),
            },
        )
    }

    async fn execute(&self, job: PulseJob, triggered_at: Instant) -> JobReport {
        match job {
            PulseJob::Usage => {
                let Ok(profiles) = self.profiles().await else {
                    return JobReport::one_failure();
                };
                match self.collectors.usage(profiles, triggered_at).await {
                    Ok(collected) => self.persist_usage(collected).await,
                    Err(_) => JobReport::one_failure(),
                }
            }
            PulseJob::Context => {
                let Ok(profiles) = self.profiles().await else {
                    return JobReport::one_failure();
                };
                match self.collectors.context(profiles, triggered_at).await {
                    Ok(collected) => self.persist_context(collected).await,
                    Err(_) => JobReport::one_failure(),
                }
            }
            PulseJob::Tokens => {
                let Ok(profiles) = self.profiles().await else {
                    return JobReport::one_failure();
                };
                let request = TokenCollectionRequest {
                    collected_at: triggered_at,
                    lookback_days: self.token_lookback_days,
                };
                self.collect_and_persist_tokens(profiles, request, false)
                    .await
            }
            PulseJob::Gemini => {
                let Ok(profiles) = self.profiles().await else {
                    return JobReport::one_failure();
                };
                match self.collectors.gemini(profiles, triggered_at).await {
                    Ok(collected) => self.persist_gemini(collected).await,
                    Err(_) => JobReport::one_failure(),
                }
            }
            PulseJob::Retention => match self
                .sink
                .retention(triggered_at, self.retention.clone())
                .await
            {
                Ok(_) => JobReport::one_success(),
                Err(_) => JobReport::one_failure(),
            },
            PulseJob::CompletionPush => {
                if let Some(reporter) = &self.completion_reporter {
                    let mut cancellation = reporter.cancellation.clone();
                    let outcomes = reporter
                        .coordinator
                        .report_completed(triggered_at, &mut cancellation)
                        .await;
                    reporter_outcomes_to_job_report(&outcomes)
                } else {
                    match self.collectors.completion_push(triggered_at).await {
                        Ok(()) => JobReport::one_success(),
                        Err(_) => JobReport::one_failure(),
                    }
                }
            }
            PulseJob::ForcePoll(target) => self.force_poll(target, triggered_at).await,
        }
    }
}

impl JobRunner for PersistingJobRunner {
    fn run(&self, job: PulseJob, triggered_at: Instant) -> JobFuture {
        let runner = self.clone();
        Box::pin(async move { Ok(runner.execute(job, triggered_at).await) })
    }
}

impl Clone for PersistingJobRunner {
    fn clone(&self) -> Self {
        Self {
            profiles: Arc::clone(&self.profiles),
            collectors: Arc::clone(&self.collectors),
            sink: Arc::clone(&self.sink),
            retention: self.retention.clone(),
            token_lookback_days: self.token_lookback_days,
            completion_reporter: self.completion_reporter.clone(),
        }
    }
}

fn reporter_outcomes_to_job_report(outcomes: &[AccountReporterOutcome]) -> JobReport {
    if outcomes.is_empty() {
        return JobReport::one_success();
    }
    outcomes
        .iter()
        .fold(JobReport::default(), |report, outcome| {
            let result = if outcome.result.is_ok() {
                JobReport::one_success()
            } else {
                JobReport::one_failure()
            };
            report.combine(result)
        })
}

async fn persist_rows<T>(
    collected: Collected<T>,
    persist: impl Fn(T) -> SinkFuture<()>,
) -> JobReport {
    let mut report = JobReport {
        attempted: collected.items.len().saturating_add(collected.failures),
        succeeded: 0,
        failed: collected.failures,
    };
    for row in collected.items {
        if persist(row).await.is_ok() {
            report.succeeded = report.succeeded.saturating_add(1);
        } else {
            report.failed = report.failed.saturating_add(1);
        }
    }
    report
}

async fn persist_observed_tokens(
    collected: Collected<TokenGrain>,
    observations: &BTreeMap<(AccountId, super::ProfileName, MachineName), TokenWriteObservation>,
    sink: &dyn PulseSink,
) -> JobReport {
    let mut report = JobReport {
        attempted: collected.items.len().saturating_add(collected.failures),
        succeeded: 0,
        failed: collected.failures,
    };
    for row in collected.items {
        let key = (row.account_id, row.profile.clone(), row.machine.clone());
        let Some(observation) = observations.get(&key).cloned() else {
            report.failed = report.failed.saturating_add(1);
            continue;
        };
        if sink.observed_tokens(observation, row).await.is_ok() {
            report.succeeded = report.succeeded.saturating_add(1);
        } else {
            report.failed = report.failed.saturating_add(1);
        }
    }
    report
}

/// Running embedded service.
pub struct PulseService {
    scheduler: SchedulerHandle,
    reporter_shutdown: Option<watch::Sender<bool>>,
    collection_enabled: bool,
}

/// Cloneable, command-only management seam for the sole Pulse scheduler.
#[derive(Clone)]
pub struct PulseManagement {
    scheduler: super::scheduler::SchedulerClient,
    collection_enabled: bool,
}

impl PulseManagement {
    /// Coalesces one explicit account- or profile-scoped force poll.
    ///
    /// # Errors
    ///
    /// Returns a conflict when collection is disabled or the scheduler is gone.
    pub fn force_poll(&self, target: ForcePollTarget) -> PulseResult<()> {
        if !self.collection_enabled {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "Pulse collection is not enabled",
            ));
        }
        self.scheduler.notify_force_poll(target)
    }
}

/// One web-owned Pulse runtime. REST/MCP and the optional collector share the
/// same Store handle; dropping API capability never starts another scheduler.
pub struct PulseRuntime {
    store: Arc<dyn Store>,
    accounts: Arc<[AccountId]>,
    invalidations: PulseInvalidationHub,
    service: Option<PulseService>,
    reset_scheduler: Option<ResetResumeScheduler>,
    receiver: Option<Arc<IngestReceiver>>,
    federation_exporter: Arc<DirectFederationExporter>,
    federation_pull: Option<FederationPullLifecycle>,
}

impl PulseRuntime {
    #[must_use]
    pub fn store(&self) -> Arc<dyn Store> {
        Arc::clone(&self.store)
    }

    #[must_use]
    pub fn accounts(&self) -> Arc<[AccountId]> {
        Arc::clone(&self.accounts)
    }

    #[must_use]
    pub fn invalidations(&self) -> PulseInvalidationHub {
        self.invalidations.clone()
    }

    /// Returns the separately authenticated receiver only when explicitly enabled.
    #[must_use]
    pub fn receiver(&self) -> Option<Arc<IngestReceiver>> {
        self.receiver.as_ref().map(Arc::clone)
    }

    /// Returns the direct, local-row-only federation exporter.
    #[must_use]
    pub fn federation_exporter(&self) -> Arc<DirectFederationExporter> {
        Arc::clone(&self.federation_exporter)
    }

    /// Returns a command-only management seam when the scheduler is running.
    #[must_use]
    pub fn management(&self) -> Option<PulseManagement> {
        self.service.as_ref().map(PulseService::management)
    }

    fn start_federation_pull(
        &mut self,
        remotes: Vec<Arc<RemoteMachine>>,
        interval: Duration,
    ) -> PulseResult<()> {
        if self.federation_pull.is_some() {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "Pulse federation pull lifecycle is already running",
            ));
        }
        let machines = remotes
            .iter()
            .map(|remote| MachineName::new(remote.id.clone()))
            .collect::<PulseResult<Vec<_>>>()?;
        let transport = Arc::new(AtmuxPullTransport::new(remotes)?);
        let consumer = Arc::new(
            StoreFederationConsumer::new(Arc::clone(&self.store))
                .with_invalidations(self.invalidations.clone()),
        );
        let federation = Arc::new(PulseFederation::new(transport, consumer));
        self.federation_pull = Some(FederationPullLifecycle::start(
            federation,
            Arc::clone(&self.accounts),
            machines.into(),
            interval,
        ));
        Ok(())
    }

    /// Coalesces a completion push when collection is active.
    ///
    /// # Errors
    ///
    /// Returns a conflict when Pulse is API-only or already shutting down.
    pub fn notify_completion(&self) -> PulseResult<()> {
        self.service.as_ref().map_or_else(
            || {
                Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse collection is not enabled",
                ))
            },
            PulseService::notify_completion,
        )
    }

    /// Stops the one optional scheduler. The shared Store is then dropped with
    /// this runtime after in-flight API requests release their clones.
    pub async fn shutdown(self) {
        if let Some(federation_pull) = self.federation_pull {
            federation_pull.shutdown().await;
        }
        if let Some(service) = self.service {
            service.shutdown().await;
        }
        if let Some(reset_scheduler) = self.reset_scheduler {
            reset_scheduler.shutdown().await;
        }
    }
}

impl PulseService {
    fn management(&self) -> PulseManagement {
        PulseManagement {
            scheduler: self.scheduler.client(),
            collection_enabled: self.collection_enabled,
        }
    }
    /// Starts an explicitly enabled service with injected typed boundaries.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an invalid schedule.
    pub fn start(
        config: &PulseConfig,
        profiles: Arc<dyn ProfileSource>,
        collectors: Arc<dyn PulseCollectors>,
        sink: Arc<dyn PulseSink>,
    ) -> PulseResult<Option<Self>> {
        Self::start_with_clock(
            config,
            profiles,
            collectors,
            sink,
            Arc::new(SystemClock::new()),
            process_jitter_seed(),
        )
    }

    /// Deterministic clock/seed injection used by integration tests.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an invalid schedule.
    pub fn start_with_clock(
        config: &PulseConfig,
        profiles: Arc<dyn ProfileSource>,
        collectors: Arc<dyn PulseCollectors>,
        sink: Arc<dyn PulseSink>,
        clock: Arc<dyn SchedulerClock>,
        jitter_seed: u64,
    ) -> PulseResult<Option<Self>> {
        Self::start_with_clock_and_reporter(
            config,
            profiles,
            collectors,
            sink,
            clock,
            jitter_seed,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_clock_and_reporter(
        config: &PulseConfig,
        profiles: Arc<dyn ProfileSource>,
        collectors: Arc<dyn PulseCollectors>,
        sink: Arc<dyn PulseSink>,
        clock: Arc<dyn SchedulerClock>,
        jitter_seed: u64,
        completion_reporter: Option<Arc<StoreReporterCoordinator>>,
    ) -> PulseResult<Option<Self>> {
        if !config.collect && !config.serve {
            return Ok(None);
        }
        config.validate()?;
        let reporter_enabled = completion_reporter.is_some();
        let settings = SchedulerSettings::from_config(&config.schedule)?
            .with_capabilities(config.collect, reporter_enabled);
        let mut runner = PersistingJobRunner::new(
            profiles,
            collectors,
            sink,
            config.retention.clone(),
            config.schedule.token_lookback_days,
        );
        let (reporter_shutdown, reporter_cancellation) = if completion_reporter.is_some() {
            let (sender, receiver) = watch::channel(false);
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        if let (Some(coordinator), Some(cancellation)) =
            (completion_reporter, reporter_cancellation)
        {
            runner = runner.with_completion_reporter(coordinator, cancellation);
        }
        Ok(Some(Self {
            scheduler: spawn_scheduler(Arc::new(runner), clock, settings, jitter_seed),
            reporter_shutdown,
            collection_enabled: config.collect,
        }))
    }

    /// Coalesces a completion-triggered reporter push.
    ///
    /// # Errors
    ///
    /// Returns a conflict error after shutdown.
    pub fn notify_completion(&self) -> PulseResult<()> {
        if !self.collection_enabled {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "Pulse collection is not enabled",
            ));
        }
        self.scheduler.notify_completion()
    }

    /// Cancels all collector work and waits for scheduler shutdown.
    pub async fn shutdown(self) {
        if let Some(reporter_shutdown) = self.reporter_shutdown {
            let _ = reporter_shutdown.send(true);
        }
        self.scheduler.shutdown().await;
    }
}

/// Opens the one web-owned Store for serving and/or collection, seeds only
/// explicitly configured accounts/profiles, and derives each local machine row
/// from the caller-provided node id. No hostname or forwarded identity is used.
///
/// # Errors
///
/// Returns a secret-free configuration/storage error when the selected store
/// cannot be opened.
pub async fn start_embedded(
    config: &PulseConfig,
    node_id: &str,
) -> PulseResult<Option<PulseRuntime>> {
    start_embedded_with_notification_factory(config, node_id, |_| Arc::new(UnavailableAlertSink))
        .await
}

/// Starts the embedded runtime with a capability-bearing notification sink.
/// The default caller uses [`UnavailableAlertSink`]; only a live negotiated
/// channel adapter should pass an available sink here.
///
/// # Errors
///
/// Returns configuration or storage failures while opening/bootstraping Pulse.
pub async fn start_embedded_with_notifications(
    config: &PulseConfig,
    node_id: &str,
    notifications: Arc<dyn PulseNotificationSink>,
) -> PulseResult<Option<PulseRuntime>> {
    start_embedded_with_notification_factory(config, node_id, |_| notifications).await
}

/// Starts the embedded runtime with pane delivery routed through the same
/// control plane as the web server. The sink revalidates each durable,
/// account-scoped subscription before resolving its pane owner. The stateless
/// MCP transport still has no negotiated channel capability.
///
/// # Errors
///
/// Returns configuration or storage failures while opening/bootstraping Pulse.
pub async fn start_embedded_with_control_plane(
    config: &PulseConfig,
    node_id: &str,
    control: ControlPlane,
) -> PulseResult<Option<PulseRuntime>> {
    let remotes = control
        .remote_machines()
        .into_iter()
        .filter(|remote| remote.id != node_id)
        .collect::<Vec<_>>();
    let mut runtime = start_embedded_with_notification_factory(config, node_id, move |store| {
        Arc::new(ControlPlaneAlertSink::new(control, store, None))
    })
    .await?;
    if let Some(runtime) = runtime.as_mut()
        && should_start_federation_pull(runtime.accounts.len(), remotes.len())
    {
        runtime.start_federation_pull(
            remotes,
            Duration::from_secs(config.effective_federation_interval_seconds()),
        )?;
    }
    Ok(runtime)
}

const fn should_start_federation_pull(account_count: usize, remote_count: usize) -> bool {
    account_count > 0 && remote_count > 0
}

async fn start_embedded_with_notification_factory<F>(
    config: &PulseConfig,
    node_id: &str,
    notification_factory: F,
) -> PulseResult<Option<PulseRuntime>>
where
    F: FnOnce(Arc<dyn Store>) -> Arc<dyn PulseNotificationSink>,
{
    if !config.collect && !config.serve {
        return Ok(None);
    }
    config.validate()?;
    let store = open_store(&config.database).await?;
    let notifications = notification_factory(Arc::clone(&store));
    let (accounts, profiles) = bootstrap_store(store.as_ref(), config, node_id).await?;
    let _profiles = preflight_configured_profiles(store.as_ref(), config, profiles).await;
    super::pricing::seed_authoritative_pricing(store.as_ref()).await?;
    let accounts: Arc<[AccountId]> = accounts.into();
    let invalidations = PulseInvalidationHub::new(accounts.as_ref());
    let collectors: Arc<dyn PulseCollectors> = if config.collect {
        Arc::new(NativeCollectors::new(
            MachineName::new(node_id.to_owned())?,
            config.credentials.clone(),
        )?)
    } else {
        Arc::new(EmptyCollectors)
    };
    let completion_reporter = build_completion_reporter(
        config,
        Arc::clone(&store),
        Arc::clone(&accounts),
        MachineName::new(node_id.to_owned())?,
    )?;
    let service = PulseService::start_with_clock_and_reporter(
        config,
        Arc::new(StoreProfileSource {
            store: Arc::clone(&store),
            accounts: Arc::clone(&accounts),
        }),
        collectors,
        Arc::new(StoreSink::with_runtime_invalidations(
            Arc::clone(&store),
            Arc::clone(&notifications),
            invalidations.clone(),
            Arc::clone(&accounts),
        )),
        Arc::new(SystemClock::new()),
        process_jitter_seed(),
        completion_reporter,
    )?;
    let reset_scheduler = config
        .collect
        .then(|| {
            let reset_sink: Arc<dyn ResetNotificationSink> = notifications;
            ResetResumeScheduler::start(
                Arc::clone(&store),
                Arc::clone(&accounts),
                reset_sink,
                Arc::new(SystemClock::new()),
                ResetResumeLimits::default(),
            )
        })
        .transpose()?;
    let receiver = config
        .receive
        .then(|| IngestReceiver::new(true, Arc::clone(&store), IngestLimits::default()))
        .transpose()?
        .map(|receiver| receiver.with_invalidations(invalidations.clone()))
        .map(Arc::new);
    let local_machine = MachineName::new(node_id.to_owned())?;
    let federation_exporter = Arc::new(DirectFederationExporter::new(
        local_machine.clone(),
        Arc::new(StoreFederationSource::new(
            Arc::clone(&store),
            accounts.as_ref(),
            local_machine,
        )),
    ));
    Ok(Some(PulseRuntime {
        store,
        accounts,
        invalidations,
        service,
        reset_scheduler,
        receiver,
        federation_exporter,
        federation_pull: None,
    }))
}

fn build_completion_reporter(
    config: &PulseConfig,
    store: Arc<dyn Store>,
    accounts: Arc<[AccountId]>,
    machine: MachineName,
) -> PulseResult<Option<Arc<StoreReporterCoordinator>>> {
    if !config.collect {
        return Ok(None);
    }
    let Some(endpoint) = config.report_to.clone() else {
        return Ok(None);
    };
    let token = match (&config.report_token_env, &config.report_token_file) {
        (Some(name), None) => SecretRef::Environment { name: name.clone() },
        (None, Some(path)) => SecretRef::File { path: path.clone() },
        _ => {
            return Err(PulseError::configuration(
                "pulse.report_to requires exactly one external report token reference",
            ));
        }
    };
    let transport = Arc::new(HttpReporterTransport::new()?);
    let node_token = match (
        &config.report_node_token_env,
        &config.report_node_token_file,
    ) {
        (Some(name), None) => Some(SecretRef::Environment { name: name.clone() }),
        (None, Some(path)) => Some(SecretRef::File { path: path.clone() }),
        (None, None) => None,
        _ => {
            return Err(PulseError::configuration(
                "configure exactly one external node token reference",
            ));
        }
    };
    let reporter = Arc::new(if let Some(node_token) = node_token {
        PulseReporter::new_with_node_token(
            endpoint,
            token,
            node_token,
            transport,
            ReporterBackoff::default(),
        )?
    } else {
        PulseReporter::new(endpoint, token, transport, ReporterBackoff::default())?
    });
    Ok(Some(Arc::new(StoreReporterCoordinator::new(
        store, accounts, machine, reporter,
    ))))
}

async fn preflight_configured_profiles(
    store: &dyn Store,
    config: &PulseConfig,
    profiles: Vec<Profile>,
) -> Vec<Profile> {
    if !config.collect || profiles.is_empty() {
        return profiles;
    }
    let Some(user_dirs) = UserDirs::new() else {
        return profiles;
    };
    let home = user_dirs.home_dir().to_path_buf();
    let wrapper_dirs = vec![home.join(".local/bin"), PathBuf::from("/usr/local/bin")];
    let inspected = profiles.clone();
    let heal = config.credentials.heal_config_dir;
    let Ok((inspected, diagnostics)) = tokio::task::spawn_blocking(move || {
        let mut inspected = inspected;
        let diagnostics =
            preflight_profiles(&mut inspected, Instant::now(), heal, &home, &wrapper_dirs);
        (inspected, diagnostics)
    })
    .await
    else {
        return profiles;
    };
    for (profile, diagnostic) in inspected.iter().zip(&diagnostics) {
        let state = diagnostic
            .credential_state
            .map_or_else(|| "not-applicable".to_owned(), |state| format!("{state:?}"));
        eprintln!(
            "atmux Pulse preflight: profile={} vendor={:?} config_dir={} credential_state={} healthy={}",
            profile.name,
            profile.vendor,
            profile
                .config_dir
                .as_deref()
                .map_or_else(|| "unset".to_owned(), |path| path.display().to_string()),
            state,
            diagnostic.healthy,
        );
        if diagnostic.healed
            && let Err(error) = store.upsert_profile(profile.clone()).await
        {
            eprintln!(
                "atmux Pulse preflight: profile={} effective config-dir repair could not be persisted: {}",
                profile.name,
                error.message()
            );
        }
    }
    inspected
}

async fn bootstrap_store(
    store: &dyn Store,
    config: &PulseConfig,
    node_id: &str,
) -> PulseResult<(Vec<AccountId>, Vec<Profile>)> {
    let machine = MachineName::new(node_id.to_owned())?;
    let now = Instant::now();
    let mut accounts = Vec::with_capacity(config.accounts.len());
    let mut profiles = Vec::new();
    for configured in &config.accounts {
        let account = configured.account()?;
        let account_id = account.id;
        store.upsert_account(account).await?;
        store
            .upsert_machine(Machine {
                account_id,
                name: machine.clone(),
                first_seen: now,
                last_seen: now,
            })
            .await?;
        for profile in configured.domain_profiles()? {
            store.upsert_profile(profile.clone()).await?;
            profiles.push(profile);
        }
        accounts.push(account_id);
    }
    Ok((accounts, profiles))
}

async fn open_store(config: &super::config::PulseDatabaseConfig) -> PulseResult<Arc<dyn Store>> {
    if let Some(environment) = &config.postgres_url_env {
        let connection = std::env::var(environment).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Configuration,
                "Pulse PostgreSQL credential reference is unavailable",
            )
        })?;
        #[cfg(feature = "pulse-postgres")]
        {
            return Ok(Arc::new(
                super::store::PostgresStore::connect(&connection).await?,
            ));
        }
        #[cfg(not(feature = "pulse-postgres"))]
        {
            let _ = connection;
            return Err(PulseError::configuration(
                "PostgreSQL requires the pulse-postgres build feature",
            ));
        }
    }
    let path = config
        .sqlite_path
        .clone()
        .map_or_else(default_sqlite_path, Ok)?;
    create_store_parent(&path).await?;
    Ok(Arc::new(SqliteStore::open(path).await?))
}

fn default_sqlite_path() -> PulseResult<PathBuf> {
    let directories = ProjectDirs::from("dev", "ryanmurf", "atmux")
        .ok_or_else(|| PulseError::configuration("could not determine the Pulse data directory"))?;
    Ok(directories.data_dir().join("pulse.sqlite3"))
}

async fn create_store_parent(path: &Path) -> PulseResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| PulseError::configuration("Pulse SQLite path has no parent"))?
        .to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::create_dir_all(parent))
        .await
        .map_err(|_| PulseError::new(PulseErrorKind::Storage, "Pulse data task failed"))?
        .map_err(|_| {
            PulseError::new(
                PulseErrorKind::Storage,
                "Pulse data directory could not be created",
            )
        })
}

fn process_jitter_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let low = u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or(0);
    low ^ u64::from(std::process::id()).rotate_left(23)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pulse::{
        AlertDelivery, AlertSubscription, AlertType, CollectionOutcome, Percent, ProfileName,
        QuotaWindow, QuotaWindowKind, RefreshPolicy, Vendor,
        alerts::{
            AlertDeliveryFuture, AlertNotification, AlertNotificationSink, PaneAlertDestination,
        },
        reset::{ResetDeliveryFuture, ResetNotification, ResetNotificationSink},
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ObservingSink {
        store: Arc<dyn Store>,
        observed_durable_state: Arc<AtomicBool>,
    }

    impl AlertNotificationSink for ObservingSink {
        fn channel_available(&self, _account_id: AccountId) -> bool {
            true
        }

        fn notify_channel(&self, notification: AlertNotification) -> AlertDeliveryFuture {
            let store = Arc::clone(&self.store);
            let observed = Arc::clone(&self.observed_durable_state);
            Box::pin(async move {
                let snapshots = store
                    .usage_history(notification.account_id, notification.profile, None, 10)
                    .await?;
                let events = store
                    .list_alert_events(notification.account_id, None)
                    .await?;
                if snapshots.is_empty()
                    || !events.iter().any(|event| event.id == notification.event_id)
                {
                    return Err(PulseError::new(
                        PulseErrorKind::Internal,
                        "notification preceded durable state",
                    ));
                }
                observed.store(true, Ordering::SeqCst);
                Ok(())
            })
        }

        fn notify_pane(
            &self,
            _destination: PaneAlertDestination,
            _notification: AlertNotification,
        ) -> AlertDeliveryFuture {
            Box::pin(async { Ok(()) })
        }
    }

    impl ResetNotificationSink for ObservingSink {
        fn channel_available(&self, _account_id: AccountId) -> bool {
            false
        }

        fn notify_reset(&self, _notification: ResetNotification) -> ResetDeliveryFuture {
            Box::pin(async { Ok(()) })
        }
    }

    fn instant(value: i64) -> Instant {
        Instant::from_epoch_millis(value).expect("instant")
    }

    #[test]
    fn federation_pull_requires_both_explicit_accounts_and_configured_remotes() {
        assert!(!should_start_federation_pull(0, 0));
        assert!(!should_start_federation_pull(1, 0));
        assert!(!should_start_federation_pull(0, 1));
        assert!(should_start_federation_pull(1, 1));
    }

    #[tokio::test]
    async fn disabled_pulse_starts_no_runtime_or_federation_task() {
        assert!(
            start_embedded(&PulseConfig::default(), "tron")
                .await
                .expect("disabled Pulse")
                .is_none()
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn store_sink_notifies_only_after_snapshot_and_event_are_durable() {
        let directory = std::env::temp_dir().join(format!(
            "atmux-alert-order-{}-{}",
            std::process::id(),
            Instant::now().epoch_millis()
        ));
        std::fs::create_dir(&directory).expect("create private test directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .expect("protect test directory");
        }
        let path = directory.join("pulse.sqlite3");
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&path).await.expect("store"));
        let account_id = AccountId::new(1).expect("account");
        let profile_name = ProfileName::new("claude").expect("profile");
        let machine_name = MachineName::new("max").expect("machine");
        store
            .upsert_account(super::super::Account {
                id: account_id,
                identity: "alerts@example.test".to_owned(),
                display_name: None,
            })
            .await
            .expect("account");
        store
            .upsert_machine(Machine {
                account_id,
                name: machine_name.clone(),
                first_seen: instant(1),
                last_seen: instant(1),
            })
            .await
            .expect("machine");
        store
            .upsert_profile(Profile {
                account_id,
                name: profile_name.clone(),
                vendor: Vendor::AnthropicOauth,
                config_dir: Some(PathBuf::from("/tmp/claude")),
                poll_interval_minutes: 15,
                monthly_budget_usd: None,
                api_key_env: None,
                api_key_file: None,
                refresh: RefreshPolicy::Never,
                hidden: false,
                origin: ProfileOrigin::Local,
            })
            .await
            .expect("profile");
        store
            .create_alert_subscription(
                AlertSubscription {
                    account_id,
                    profile: profile_name.clone(),
                    alert_type: AlertType::FiveHourThreshold,
                    threshold: Some(Percent::new(80.0).expect("threshold")),
                    cooldown_minutes: 30,
                    delivery: Some(AlertDelivery::Channel),
                    enabled: true,
                },
                instant(1),
            )
            .await
            .expect("subscription");
        let observed = Arc::new(AtomicBool::new(false));
        let notifications = Arc::new(ObservingSink {
            store: Arc::clone(&store),
            observed_durable_state: Arc::clone(&observed),
        });
        let invalidations = PulseInvalidationHub::new(&[account_id]);
        let mut subscription = invalidations.subscribe(account_id).expect("subscription");
        let sink = StoreSink::with_runtime_invalidations(
            store,
            notifications,
            invalidations,
            Arc::from([account_id]),
        );
        sink.usage(UsageSnapshot {
            account_id,
            profile: profile_name,
            machine: machine_name,
            vendor: Vendor::AnthropicOauth,
            windows: vec![QuotaWindow {
                kind: QuotaWindowKind::FiveHour,
                used_percent: Percent::new(90.0).expect("usage"),
                resets_at: instant(100_000),
            }],
            outcome: CollectionOutcome::Success,
            polled_at: instant(10_000),
            reporter_version: None,
        })
        .await
        .expect("persist and notify");
        assert!(observed.load(Ordering::SeqCst));
        subscription.receiver.changed().await.expect("invalidation");
        assert_eq!(*subscription.receiver.borrow_and_update(), 1);
        sink.retention(instant(200_000_000), PulseRetentionConfig::default())
            .await
            .expect("retention");
        subscription
            .receiver
            .changed()
            .await
            .expect("retention invalidation");
        assert_eq!(*subscription.receiver.borrow_and_update(), 2);
        drop(sink);
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = std::fs::remove_file(candidate);
        }
        let _ = std::fs::remove_dir(directory);
    }
}
