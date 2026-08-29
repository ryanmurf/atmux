//! Bounded, account-scoped Pulse command operations.
//!
//! These operations are deliberately one-shot. They never spawn the Pulse
//! scheduler, start a daemon, delete rows, or accept credential values. CLI
//! adapters supply an explicit configured account and a cancellation receiver.

use std::{
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use directories::{ProjectDirs, UserDirs};
use tokio::sync::watch;

use super::{
    AccountId, Instant, Machine, MachineName, Profile, ProfileName, ProfileOrigin, PulseConfig,
    PulseError, PulseErrorKind, PulseResult, TokenGrain, Vendor,
    collect::SecretRef,
    health::{GaugeHealth, ProfileCredentialHealth, collect_gauge_health},
    ingest::{MAX_PUSH_ROWS, PushBatch, ReportedProfile},
    native::NativeCollectors,
    preflight::preflight_profiles,
    reporter::{HttpReporterTransport, PulseReporter, ReporterBackoff, ReporterOutcome},
    scheduler::{JobReport, JobRunner, PulseJob},
    service::{
        PersistingJobRunner, ProfileFuture, ProfileSource, PulseCollectors, PulseSink, StoreSink,
    },
    store::{SqliteStore, Store, TokenBackfillPage},
    token::{
        TokenSourceGeneration, TokenTallyCursor, TokenTallyPage, tally_profile_page,
        token_source_generation,
    },
};

const MAX_WRAPPER_DIRS: usize = 8;
const MAX_FULL_HISTORY_ROWS_PER_PROFILE: usize = 5_000;
const MAX_BACKFILL_GENERATION_RESTARTS: usize = 3;

/// Explicit, bounded locations used only for credential preflight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalProfilePaths {
    home: PathBuf,
    wrapper_dirs: Vec<PathBuf>,
}

impl LocalProfilePaths {
    /// Creates validated local discovery paths. Paths are never executed.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for a relative/non-normal path or more
    /// than eight wrapper search directories.
    pub fn new(home: PathBuf, wrapper_dirs: Vec<PathBuf>) -> PulseResult<Self> {
        if !safe_absolute(&home) {
            return Err(PulseError::configuration(
                "Pulse operational home path must be absolute and normalized",
            ));
        }
        if wrapper_dirs.len() > MAX_WRAPPER_DIRS
            || wrapper_dirs.iter().any(|path| !safe_absolute(path))
        {
            return Err(PulseError::configuration(
                "Pulse operational wrapper paths are invalid or exceed their bound",
            ));
        }
        Ok(Self { home, wrapper_dirs })
    }

    /// Uses the current user's home with the same fixed wrapper locations as
    /// the embedded service.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the platform has no user directory.
    pub fn current() -> PulseResult<Self> {
        let directories = UserDirs::new().ok_or_else(|| {
            PulseError::configuration("could not determine the Pulse user directory")
        })?;
        let home = directories.home_dir().to_path_buf();
        Self::new(
            home.clone(),
            vec![home.join(".local/bin"), PathBuf::from("/usr/local/bin")],
        )
    }

    fn home(&self) -> &Path {
        &self.home
    }

    fn wrapper_dirs(&self) -> &[PathBuf] {
        &self.wrapper_dirs
    }
}

/// Inputs for one read-only `pulse doctor` pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorRequest {
    pub account_id: AccountId,
    pub machine: MachineName,
    pub now: Instant,
    pub paths: LocalProfilePaths,
}

/// Availability of one explicitly externalized secret reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalSecretHealth {
    NotConfigured,
    Available,
    Unavailable,
}

/// One configured profile's secret-free doctor result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorProfile {
    pub account_id: AccountId,
    pub profile: ProfileName,
    pub vendor: Vendor,
    pub persisted: bool,
    pub configuration_matches: bool,
    pub credential: ProfileCredentialHealth,
    pub preflight_healthy: bool,
    pub gauge: GaugeHealth,
    pub last_polled_at: Option<Instant>,
}

/// Bounded, secret-free result of one account-scoped doctor pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorResult {
    pub account_id: AccountId,
    pub machine: MachineName,
    pub schema_version: u32,
    pub integrity_ok: bool,
    pub report_ingest_secret: ExternalSecretHealth,
    pub report_node_secret: ExternalSecretHealth,
    pub profiles: Vec<DoctorProfile>,
}

/// Diagnoses only one explicitly configured account without changing storage.
///
/// The existing gauge classifier keeps dead, authentication-failed, null,
/// stale, authenticated-but-unchanged, and healthy observations distinct.
/// Credential values and provider response bodies never enter the result.
///
/// # Errors
///
/// Returns a configuration error for invalid Pulse configuration, not-found
/// for an unconfigured account, conflict if the database account identity or
/// profile origin/vendor disagrees with configuration, or a bounded store
/// error. No sibling account is queried.
pub async fn doctor(
    config: &PulseConfig,
    store: &dyn Store,
    request: DoctorRequest,
) -> PulseResult<DoctorResult> {
    config.validate()?;
    let configured = configured_account(config, request.account_id)?;
    let account = configured.account()?;
    if let Some(stored) = store.get_account(request.account_id).await?
        && stored.identity != account.identity
    {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "configured Pulse account identity conflicts with stored data",
        ));
    }

    let mut profiles = configured
        .domain_profiles()?
        .into_iter()
        .filter(|profile| profile.origin == ProfileOrigin::Local)
        .collect::<Vec<_>>();
    let mut persisted = Vec::with_capacity(profiles.len());
    for profile in &profiles {
        let stored = store
            .get_profile(request.account_id, profile.name.clone())
            .await?;
        if stored.as_ref().is_some_and(|stored| {
            stored.origin != ProfileOrigin::Local || stored.vendor != profile.vendor
        }) {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "configured Pulse profile conflicts with stored origin or vendor",
            ));
        }
        persisted.push(stored);
    }

    let inspected = profiles.clone();
    let now = request.now;
    let home = request.paths.home().to_path_buf();
    let wrappers = request.paths.wrapper_dirs().to_vec();
    let preflight = tokio::task::spawn_blocking(move || {
        let mut inspected = inspected;
        preflight_profiles(&mut inspected, now, false, &home, &wrappers)
    })
    .await
    .map_err(|_| PulseError::new(PulseErrorKind::Internal, "Pulse doctor task failed"))?;
    let gauge = collect_gauge_health(store, &profiles, &request.machine, request.now).await?;
    if preflight.len() != profiles.len() || gauge.len() != profiles.len() {
        return Err(PulseError::new(
            PulseErrorKind::Internal,
            "Pulse doctor returned an inconsistent profile set",
        ));
    }

    let profiles = profiles
        .drain(..)
        .zip(persisted)
        .zip(preflight)
        .zip(gauge)
        .map(|(((configured, stored), preflight), gauge)| {
            let configuration_matches = stored.as_ref() == Some(&configured);
            DoctorProfile {
                account_id: request.account_id,
                profile: configured.name,
                vendor: configured.vendor,
                persisted: stored.is_some(),
                configuration_matches,
                credential: gauge.credential,
                preflight_healthy: preflight.healthy,
                gauge: gauge.gauge,
                last_polled_at: gauge.last_polled_at,
            }
        })
        .collect();

    Ok(DoctorResult {
        account_id: request.account_id,
        machine: request.machine,
        schema_version: store.schema_version().await?,
        integrity_ok: store.integrity_check().await?.eq_ignore_ascii_case("ok"),
        report_ingest_secret: secret_health(report_token_ref(config)),
        report_node_secret: secret_health(report_node_token_ref(config)),
        profiles,
    })
}

/// Inputs for exactly one explicit local collection and optional report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushOnceRequest {
    pub account_id: AccountId,
    pub machine: MachineName,
    pub started_at: Instant,
    pub backfill: bool,
    pub restart_backfill: bool,
    pub paths: LocalProfilePaths,
}

/// Per-collector accounting. `None` means cancellation happened before that
/// collector started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PushCollectionResult {
    pub usage: Option<JobReport>,
    pub context: Option<JobReport>,
    pub tokens: Option<JobReport>,
    pub gemini: Option<JobReport>,
    pub backfill_truncated: bool,
}

/// Secret-free status of the optional configured reporter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushReportResult {
    NotConfigured,
    Sent {
        chunks: usize,
        rows: usize,
        truncated: bool,
    },
    Cancelled {
        chunks: usize,
        rows: usize,
        truncated: bool,
    },
    Failed {
        kind: PulseErrorKind,
        truncated: bool,
    },
}

/// Result of one non-scheduled push operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PushOnceResult {
    pub account_id: AccountId,
    pub collections: PushCollectionResult,
    pub report: PushReportResult,
    pub cancelled: bool,
}

/// A caller-bounded full-history token result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FullHistoryRows {
    rows: Vec<TokenGrain>,
    next_cursor: Option<TokenTallyCursor>,
    complete: bool,
    source_generation: TokenSourceGeneration,
}

impl FullHistoryRows {
    /// Creates a result that cannot exceed the native per-profile bound.
    ///
    /// # Errors
    ///
    /// Returns invalid input when a backfiller exceeds its requested or native
    /// row limit.
    pub fn new(rows: Vec<TokenGrain>, requested_max: usize) -> PulseResult<Self> {
        Self::bounded_page(
            rows,
            requested_max,
            None,
            TokenSourceGeneration::new("0".repeat(64))?,
        )
    }

    /// Creates an explicitly completed or incomplete bounded page for an
    /// injected full-history collector.
    ///
    /// # Errors
    ///
    /// Returns invalid input for an invalid generation, empty incomplete page,
    /// or a result that exceeds its requested or native row limit.
    pub fn page(
        rows: Vec<TokenGrain>,
        requested_max: usize,
        complete: bool,
        source_generation: TokenSourceGeneration,
    ) -> PulseResult<Self> {
        Self::bounded_page(rows, requested_max, Some(complete), source_generation)
    }

    fn bounded_page(
        rows: Vec<TokenGrain>,
        requested_max: usize,
        complete: Option<bool>,
        source_generation: TokenSourceGeneration,
    ) -> PulseResult<Self> {
        if requested_max == 0
            || requested_max > MAX_FULL_HISTORY_ROWS_PER_PROFILE
            || rows.len() > requested_max
            || (complete == Some(false) && rows.is_empty())
        {
            return Err(PulseError::invalid_input(
                "Pulse full-history result exceeded its row bound",
            ));
        }
        source_generation.validate()?;
        let next_cursor = rows.last().map(TokenTallyCursor::from_grain);
        Ok(Self {
            complete: complete.unwrap_or(rows.len() < requested_max),
            rows,
            next_cursor,
            source_generation,
        })
    }

    fn from_page(page: TokenTallyPage, requested_max: usize) -> PulseResult<Self> {
        if requested_max == 0
            || requested_max > MAX_FULL_HISTORY_ROWS_PER_PROFILE
            || page.tally.grains.len() > requested_max
        {
            return Err(PulseError::new(
                PulseErrorKind::Internal,
                "Pulse full-history page violated its row bound",
            ));
        }
        Ok(Self {
            rows: page.tally.grains,
            next_cursor: page.next_cursor,
            complete: page.complete,
            source_generation: page.source_generation,
        })
    }
}

/// Boxed future for an injectable full-history token scan.
pub type FullHistoryFuture =
    Pin<Box<dyn Future<Output = PulseResult<FullHistoryRows>> + Send + 'static>>;
/// Boxed future for a source-generation witness without tallying rows.
pub type FullHistoryGenerationFuture =
    Pin<Box<dyn Future<Output = PulseResult<TokenSourceGeneration>> + Send + 'static>>;

/// Read-only full-history scanner used only when `--backfill` is explicit.
pub trait FullHistoryCollector: Send + Sync + 'static {
    fn source_generation(
        &self,
        profile: Profile,
        machine: MachineName,
    ) -> FullHistoryGenerationFuture;

    fn collect(
        &self,
        profile: Profile,
        machine: MachineName,
        after: Option<TokenTallyCursor>,
        max_rows: usize,
    ) -> FullHistoryFuture;
}

/// Production adapter over the bounded native token tally.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeFullHistoryCollector;

impl FullHistoryCollector for NativeFullHistoryCollector {
    fn source_generation(
        &self,
        profile: Profile,
        machine: MachineName,
    ) -> FullHistoryGenerationFuture {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || token_source_generation(&profile, &machine))
                .await
                .map_err(|_| {
                    PulseError::new(PulseErrorKind::Internal, "Pulse backfill task failed")
                })?
        })
    }

    fn collect(
        &self,
        profile: Profile,
        machine: MachineName,
        after: Option<TokenTallyCursor>,
        max_rows: usize,
    ) -> FullHistoryFuture {
        Box::pin(async move {
            let tally = tokio::task::spawn_blocking(move || {
                tally_profile_page(&profile, &machine, after.as_ref(), max_rows)
            })
            .await
            .map_err(|_| {
                PulseError::new(PulseErrorKind::Internal, "Pulse backfill task failed")
            })??;
            FullHistoryRows::from_page(tally, max_rows)
        })
    }
}

#[derive(Clone, Debug)]
struct OneShotProfiles {
    profiles: Arc<[Profile]>,
}

impl ProfileSource for OneShotProfiles {
    fn profiles(&self) -> ProfileFuture {
        let profiles = self.profiles.to_vec();
        Box::pin(async move { Ok(profiles) })
    }
}

async fn report_once(
    config: &PulseConfig,
    store: &dyn Store,
    reporter: &PulseReporter,
    request: &PushOnceRequest,
    profiles: &[Profile],
    cancellation: &mut watch::Receiver<bool>,
) -> PulseResult<(PushReportResult, bool)> {
    if cancellation_requested(cancellation) {
        return Ok((
            PushReportResult::Cancelled {
                chunks: 0,
                rows: 0,
                truncated: false,
            },
            true,
        ));
    }
    let Some((batch, truncated)) = run_cancellable(
        assemble_report_batch(
            store,
            request.account_id,
            &request.machine,
            request.started_at,
            config.schedule.token_lookback_days,
            request.backfill,
            profiles,
        ),
        cancellation,
    )
    .await?
    else {
        return Ok((
            PushReportResult::Cancelled {
                chunks: 0,
                rows: 0,
                truncated: false,
            },
            true,
        ));
    };
    let outcome = reporter
        .report_batch(
            request.account_id,
            request.machine.clone(),
            batch,
            cancellation,
        )
        .await;
    Ok(match outcome {
        Ok(ReporterOutcome {
            chunks_sent,
            rows_sent,
            cancelled: true,
        }) => (
            PushReportResult::Cancelled {
                chunks: chunks_sent,
                rows: rows_sent,
                truncated,
            },
            true,
        ),
        Ok(ReporterOutcome {
            chunks_sent,
            rows_sent,
            cancelled: false,
        }) => (
            PushReportResult::Sent {
                chunks: chunks_sent,
                rows: rows_sent,
                truncated,
            },
            false,
        ),
        Err(error) => (
            PushReportResult::Failed {
                kind: error.kind(),
                truncated,
            },
            false,
        ),
    })
}

/// Runs one injected, deterministic collection/persistence/report cycle.
///
/// This is the testable core used by [`push_once_native`]. It never starts a
/// scheduler. Only the selected account's explicitly configured local profiles
/// are collected, persisted, and admitted to the report batch.
///
/// # Errors
///
/// Returns configuration/not-found/conflict failures before provider work;
/// storage failures while preparing the explicit scope; or an internal error
/// if an injected job boundary violates its contract. Provider/persistence
/// row failures remain in typed job reports so healthy sibling profiles finish.
#[allow(clippy::too_many_arguments)]
pub async fn push_once_with(
    config: &PulseConfig,
    store: Arc<dyn Store>,
    collectors: Arc<dyn PulseCollectors>,
    sink: Arc<dyn PulseSink>,
    full_history: Arc<dyn FullHistoryCollector>,
    reporter: Option<Arc<PulseReporter>>,
    request: PushOnceRequest,
    cancellation: &mut watch::Receiver<bool>,
) -> PulseResult<PushOnceResult> {
    config.validate()?;
    validate_reporter_presence(config, reporter.as_ref())?;
    let mut result = PushOnceResult {
        account_id: request.account_id,
        collections: PushCollectionResult::default(),
        report: if reporter.is_some() {
            PushReportResult::Cancelled {
                chunks: 0,
                rows: 0,
                truncated: false,
            }
        } else {
            PushReportResult::NotConfigured
        },
        cancelled: false,
    };
    if cancellation_requested(cancellation) {
        result.cancelled = true;
        return Ok(result);
    }

    let Some(profiles) = run_cancellable(
        prepare_push_scope(config, store.as_ref(), &request),
        cancellation,
    )
    .await?
    else {
        result.cancelled = true;
        return Ok(result);
    };
    if profiles.is_empty() {
        return Err(PulseError::configuration(
            "pulse push requires at least one configured local profile",
        ));
    }
    let runner = PersistingJobRunner::new(
        Arc::new(OneShotProfiles {
            profiles: profiles.clone().into(),
        }),
        collectors,
        Arc::clone(&sink),
        config.retention.clone(),
        config.schedule.token_lookback_days,
    );

    let Some(collections) = collect_once(
        &runner,
        &profiles,
        &request,
        store.as_ref(),
        full_history.as_ref(),
        cancellation,
    )
    .await?
    else {
        result.cancelled = true;
        return Ok(result);
    };
    result.collections = collections;

    let Some(reporter) = reporter else {
        result.report = PushReportResult::NotConfigured;
        return Ok(result);
    };
    let (report, cancelled) = report_once(
        config,
        store.as_ref(),
        reporter.as_ref(),
        &request,
        &profiles,
        cancellation,
    )
    .await?;
    result.report = report;
    result.cancelled |= cancelled;
    Ok(result)
}

/// Builds native collectors and the optional externally referenced reporter,
/// then executes one bounded cycle without a scheduler.
///
/// # Errors
///
/// Returns the same errors as [`push_once_with`], plus TLS client setup errors.
pub async fn push_once_native(
    config: &PulseConfig,
    store: Arc<dyn Store>,
    request: PushOnceRequest,
    cancellation: &mut watch::Receiver<bool>,
) -> PulseResult<PushOnceResult> {
    let collectors: Arc<dyn PulseCollectors> = Arc::new(NativeCollectors::new(
        request.machine.clone(),
        config.credentials.clone(),
    )?);
    let sink: Arc<dyn PulseSink> = Arc::new(StoreSink::new(Arc::clone(&store)));
    let reporter = configured_reporter(config)?;
    push_once_with(
        config,
        store,
        collectors,
        sink,
        Arc::new(NativeFullHistoryCollector),
        reporter,
        request,
        cancellation,
    )
    .await
}

/// Opens the configured operational store using only an external `PostgreSQL`
/// URL reference or the normal local `SQLite` path.
///
/// # Errors
///
/// Returns a secret-free configuration/storage error. Connection strings are
/// never stored in an operational request or result.
pub async fn open_operational_store(config: &PulseConfig) -> PulseResult<Arc<dyn Store>> {
    config.validate()?;
    if let Some(environment) = &config.database.postgres_url_env {
        let connection = std::env::var(environment).map_err(|_| {
            PulseError::configuration(
                "Pulse PostgreSQL external credential reference is unavailable",
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
    let path = operational_sqlite_path(config)?;
    Ok(Arc::new(SqliteStore::open(path).await?))
}

/// Opens an existing, already-current store for `pulse doctor`.
///
/// The selected backend is opened read-only and must already have the exact
/// current schema. No migrations or operational writes are attempted.
///
/// # Errors
///
/// Returns not-found for a missing `SQLite` database, configuration for an
/// unsafe/outdated database, and storage for failed read-only validation.
pub async fn open_doctor_store(config: &PulseConfig) -> PulseResult<Arc<dyn Store>> {
    config.validate()?;
    let store: Arc<dyn Store> = if let Some(environment) = &config.database.postgres_url_env {
        let connection = std::env::var(environment).map_err(|_| {
            PulseError::configuration(
                "Pulse PostgreSQL external credential reference is unavailable",
            )
        })?;
        #[cfg(feature = "pulse-postgres")]
        {
            Arc::new(super::store::PostgresStore::connect_read_only(&connection).await?)
        }
        #[cfg(not(feature = "pulse-postgres"))]
        {
            let _ = connection;
            return Err(PulseError::configuration(
                "PostgreSQL requires the pulse-postgres build feature",
            ));
        }
    } else {
        let path = operational_sqlite_path(config)?;
        Arc::new(SqliteStore::open_read_only(path).await?)
    };
    let version = store
        .schema_version()
        .await
        .map_err(|_| PulseError::configuration("Pulse doctor database has no recognized schema"))?;
    if version != super::store::schema::LATEST_SCHEMA_VERSION {
        return Err(PulseError::configuration(format!(
            "Pulse doctor requires schema {}; found {version}",
            super::store::schema::LATEST_SCHEMA_VERSION
        )));
    }
    let integrity = store.integrity_check().await.map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Pulse doctor database integrity check failed",
        )
    })?;
    if !integrity.eq_ignore_ascii_case("ok") {
        return Err(PulseError::new(
            PulseErrorKind::Storage,
            "Pulse doctor database failed its integrity check",
        ));
    }
    Ok(store)
}

/// Constructs a reporter without resolving either external token reference.
///
/// # Errors
///
/// Returns a configuration error for an ambiguous/missing external reference
/// or an unsafe endpoint, and a setup error if TLS roots are unavailable.
pub fn configured_reporter(config: &PulseConfig) -> PulseResult<Option<Arc<PulseReporter>>> {
    config.validate()?;
    let Some(endpoint) = config.report_to.clone() else {
        return Ok(None);
    };
    let token = report_token_ref(config).ok_or_else(|| {
        PulseError::configuration(
            "pulse.report_to requires exactly one external report token reference",
        )
    })?;
    let node_token = report_node_token_ref(config);
    let transport = Arc::new(HttpReporterTransport::new()?);
    let reporter = if let Some(node_token) = node_token {
        PulseReporter::new_with_node_token(
            endpoint,
            token,
            node_token,
            transport,
            ReporterBackoff::default(),
        )?
    } else {
        PulseReporter::new(endpoint, token, transport, ReporterBackoff::default())?
    };
    Ok(Some(Arc::new(reporter)))
}

async fn prepare_push_scope(
    config: &PulseConfig,
    store: &dyn Store,
    request: &PushOnceRequest,
) -> PulseResult<Vec<Profile>> {
    let configured = configured_account(config, request.account_id)?;
    let account = configured.account()?;
    if let Some(stored) = store.get_account(request.account_id).await?
        && stored.identity != account.identity
    {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "configured Pulse account identity conflicts with stored data",
        ));
    }
    store.upsert_account(account).await?;

    let mut profiles = configured
        .domain_profiles()?
        .into_iter()
        .filter(|profile| profile.origin == ProfileOrigin::Local)
        .collect::<Vec<_>>();
    for profile in &profiles {
        if let Some(stored) = store
            .get_profile(request.account_id, profile.name.clone())
            .await?
            && (stored.origin != ProfileOrigin::Local || stored.vendor != profile.vendor)
        {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "configured Pulse profile conflicts with stored origin or vendor",
            ));
        }
        store.upsert_profile(profile.clone()).await?;
    }

    let inspected = profiles.clone();
    let now = request.started_at;
    let heal = config.credentials.heal_config_dir;
    let home = request.paths.home().to_path_buf();
    let wrappers = request.paths.wrapper_dirs().to_vec();
    let inspected = tokio::task::spawn_blocking(move || {
        let mut inspected = inspected;
        let diagnostics = preflight_profiles(&mut inspected, now, heal, &home, &wrappers);
        (inspected, diagnostics)
    })
    .await
    .map_err(|_| PulseError::new(PulseErrorKind::Internal, "Pulse preflight task failed"))?;
    for (profile, diagnostic) in inspected.0.into_iter().zip(inspected.1) {
        if diagnostic.healed {
            store.upsert_profile(profile.clone()).await?;
        }
        if let Some(current) = profiles
            .iter_mut()
            .find(|current| current.name == profile.name)
        {
            *current = profile;
        }
    }

    let existing = store
        .list_machines(request.account_id)
        .await?
        .into_iter()
        .find(|machine| machine.name == request.machine);
    store
        .upsert_machine(Machine {
            account_id: request.account_id,
            name: request.machine.clone(),
            first_seen: existing.map_or(request.started_at, |machine| machine.first_seen),
            last_seen: request.started_at,
        })
        .await?;
    Ok(profiles)
}

async fn collect_once(
    runner: &PersistingJobRunner,
    profiles: &[Profile],
    request: &PushOnceRequest,
    store: &dyn Store,
    full_history: &dyn FullHistoryCollector,
    cancellation: &mut watch::Receiver<bool>,
) -> PulseResult<Option<PushCollectionResult>> {
    let Some(usage) = run_job(runner, PulseJob::Usage, request.started_at, cancellation).await?
    else {
        return Ok(None);
    };
    let Some(context) =
        run_job(runner, PulseJob::Context, request.started_at, cancellation).await?
    else {
        return Ok(None);
    };
    let (tokens, backfill_truncated) = if request.backfill {
        let Some((tokens, truncated)) = run_full_history(
            profiles,
            &request.machine,
            store,
            full_history,
            request.restart_backfill,
            cancellation,
        )
        .await?
        else {
            return Ok(None);
        };
        (tokens, truncated)
    } else {
        let Some(tokens) =
            run_job(runner, PulseJob::Tokens, request.started_at, cancellation).await?
        else {
            return Ok(None);
        };
        (tokens, false)
    };
    let Some(gemini) = run_job(runner, PulseJob::Gemini, request.started_at, cancellation).await?
    else {
        return Ok(None);
    };
    Ok(Some(PushCollectionResult {
        usage: Some(usage),
        context: Some(context),
        tokens: Some(tokens),
        gemini: Some(gemini),
        backfill_truncated,
    }))
}

async fn run_job(
    runner: &PersistingJobRunner,
    job: PulseJob,
    now: Instant,
    cancellation: &mut watch::Receiver<bool>,
) -> PulseResult<Option<JobReport>> {
    if cancellation_requested(cancellation) {
        return Ok(None);
    }
    let future = runner.run(job, now);
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result.map(Some),
            changed = cancellation.changed() => {
                if changed.is_err() || *cancellation.borrow() {
                    return Ok(None);
                }
            }
        }
    }
}

async fn run_full_history(
    profiles: &[Profile],
    machine: &MachineName,
    store: &dyn Store,
    collector: &dyn FullHistoryCollector,
    restart_completed: bool,
    cancellation: &mut watch::Receiver<bool>,
) -> PulseResult<Option<(JobReport, bool)>> {
    let eligible = profiles
        .iter()
        .filter(|profile| {
            profile.origin == ProfileOrigin::Local
                && !matches!(profile.vendor, Vendor::Gemini | Vendor::XaiGrok)
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut report = JobReport::default();
    for profile in eligible {
        let Some(profile_report) = backfill_profile(
            store,
            &profile,
            machine,
            collector,
            restart_completed,
            cancellation,
        )
        .await?
        else {
            return Ok(None);
        };
        report = report.combine(profile_report);
    }
    Ok(Some((report, false)))
}

async fn backfill_profile(
    store: &dyn Store,
    profile: &Profile,
    machine: &MachineName,
    collector: &dyn FullHistoryCollector,
    restart_completed: bool,
    cancellation: &mut watch::Receiver<bool>,
) -> PulseResult<Option<JobReport>> {
    let Some(mut state) = begin_backfill_profile(
        store,
        profile,
        machine,
        collector,
        restart_completed,
        cancellation,
    )
    .await?
    else {
        return Ok(None);
    };
    if state.complete {
        return Ok(Some(JobReport::default()));
    }
    let Some(mut rows) = collect_backfill_page(
        profile,
        machine,
        state.cursor.clone(),
        collector,
        cancellation,
    )
    .await?
    else {
        return Ok(None);
    };
    let mut report = JobReport::default();
    let mut generation_restarts = 0_usize;
    loop {
        if rows.source_generation != state.source_generation {
            generation_restarts = generation_restarts.saturating_add(1);
            if generation_restarts > MAX_BACKFILL_GENERATION_RESTARTS {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse token source changed too often during backfill",
                ));
            }
            let Some(reset) = run_cancellable(
                store.begin_token_backfill(
                    profile.account_id,
                    profile.name.clone(),
                    machine.clone(),
                    rows.source_generation.clone(),
                    false,
                ),
                cancellation,
            )
            .await?
            else {
                return Ok(None);
            };
            state = reset;
            let Some(restarted) =
                collect_backfill_page(profile, machine, None, collector, cancellation).await?
            else {
                return Ok(None);
            };
            rows = restarted;
            continue;
        }
        let attempted = rows.rows.len();
        let next_cursor = rows.next_cursor.clone().or_else(|| state.cursor.clone());
        let Some(next) = run_cancellable(
            store.apply_token_backfill_page(TokenBackfillPage {
                expected: state,
                rows: rows.rows,
                next_cursor,
                complete: rows.complete,
            }),
            cancellation,
        )
        .await?
        else {
            return Ok(None);
        };
        report.attempted = report.attempted.saturating_add(attempted);
        report.succeeded = report.succeeded.saturating_add(attempted);
        state = next;
        if state.complete {
            return Ok(Some(report));
        }
        let Some(next_rows) = collect_backfill_page(
            profile,
            machine,
            state.cursor.clone(),
            collector,
            cancellation,
        )
        .await?
        else {
            return Ok(None);
        };
        rows = next_rows;
    }
}

async fn begin_backfill_profile(
    store: &dyn Store,
    profile: &Profile,
    machine: &MachineName,
    collector: &dyn FullHistoryCollector,
    restart_completed: bool,
    cancellation: &mut watch::Receiver<bool>,
) -> PulseResult<Option<super::store::TokenBackfillState>> {
    let Some(source_generation) = run_cancellable(
        collector.source_generation(profile.clone(), machine.clone()),
        cancellation,
    )
    .await?
    else {
        return Ok(None);
    };
    run_cancellable(
        store.begin_token_backfill(
            profile.account_id,
            profile.name.clone(),
            machine.clone(),
            source_generation,
            restart_completed,
        ),
        cancellation,
    )
    .await
}

async fn collect_backfill_page(
    profile: &Profile,
    machine: &MachineName,
    after: Option<TokenTallyCursor>,
    collector: &dyn FullHistoryCollector,
    cancellation: &mut watch::Receiver<bool>,
) -> PulseResult<Option<FullHistoryRows>> {
    run_cancellable(
        collector.collect(
            profile.clone(),
            machine.clone(),
            after,
            MAX_FULL_HISTORY_ROWS_PER_PROFILE,
        ),
        cancellation,
    )
    .await
}

struct ReportScope<'a> {
    account_id: AccountId,
    machine: &'a MachineName,
    started_at: Instant,
    token_since: Option<String>,
}

struct ReportAssembly {
    batch: PushBatch,
    remaining: usize,
    truncated: bool,
}

async fn assemble_report_batch(
    store: &dyn Store,
    account_id: AccountId,
    machine: &MachineName,
    started_at: Instant,
    lookback_days: u16,
    full_history: bool,
    profiles: &[Profile],
) -> PulseResult<(PushBatch, bool)> {
    let batch = PushBatch {
        profiles: profiles
            .iter()
            .map(|profile| ReportedProfile {
                name: profile.name.clone(),
                vendor: profile.vendor,
                poll_interval_minutes: profile.poll_interval_minutes,
                monthly_budget_usd: profile.monthly_budget_usd,
            })
            .collect(),
        ..PushBatch::default()
    };
    let scope = ReportScope {
        account_id,
        machine,
        started_at,
        token_since: if full_history {
            None
        } else {
            Some(recent_day(started_at, lookback_days)?)
        },
    };
    let mut assembly = ReportAssembly {
        remaining: MAX_PUSH_ROWS.saturating_sub(batch.profiles.len()),
        batch,
        truncated: false,
    };
    for profile in profiles {
        if assembly.remaining == 0 {
            assembly.truncated = true;
            break;
        }
        append_profile_report_rows(store, &scope, profile, &mut assembly).await?;
    }
    if assembly.remaining > 0 {
        let gemini = store.list_gemini_quotas(account_id).await?;
        let mut gemini = gemini
            .into_iter()
            .filter(|quota| quota.account_id == account_id && quota.collected_at == started_at)
            .collect::<Vec<_>>();
        truncate_into(
            &mut assembly.batch.gemini_quotas,
            &mut gemini,
            &mut assembly.remaining,
            &mut assembly.truncated,
        );
    } else {
        assembly.truncated = true;
    }
    Ok((assembly.batch, assembly.truncated))
}

async fn append_profile_report_rows(
    store: &dyn Store,
    scope: &ReportScope<'_>,
    profile: &Profile,
    assembly: &mut ReportAssembly,
) -> PulseResult<()> {
    let usage = store
        .usage_history(
            scope.account_id,
            profile.name.clone(),
            Some(scope.started_at),
            assembly.remaining,
        )
        .await?;
    let mut usage = usage
        .into_iter()
        .map(|stored| stored.snapshot)
        .filter(|snapshot| {
            snapshot.account_id == scope.account_id
                && snapshot.machine == *scope.machine
                && snapshot.polled_at == scope.started_at
        })
        .collect::<Vec<_>>();
    truncate_into(
        &mut assembly.batch.snapshots,
        &mut usage,
        &mut assembly.remaining,
        &mut assembly.truncated,
    );
    if assembly.remaining == 0 {
        assembly.truncated = true;
        return Ok(());
    }

    let contexts = store
        .list_context_sessions(scope.account_id, Some(profile.name.clone()))
        .await?;
    let mut contexts = contexts
        .into_iter()
        .filter(|session| {
            session.account_id == scope.account_id
                && session.profile == profile.name
                && session.machine == *scope.machine
                && session.collected_at == scope.started_at
        })
        .collect::<Vec<_>>();
    truncate_into(
        &mut assembly.batch.context_sessions,
        &mut contexts,
        &mut assembly.remaining,
        &mut assembly.truncated,
    );
    if assembly.remaining == 0 {
        assembly.truncated = true;
        return Ok(());
    }

    let tokens = store
        .list_token_grains(
            scope.account_id,
            Some(profile.name.clone()),
            scope.token_since.clone(),
            assembly.remaining,
        )
        .await?;
    let mut tokens = tokens
        .into_iter()
        .filter(|grain| {
            grain.account_id == scope.account_id
                && grain.profile == profile.name
                && grain.machine == *scope.machine
                && grain.source == super::TokenSource::Local
        })
        .collect::<Vec<_>>();
    truncate_into(
        &mut assembly.batch.token_grains,
        &mut tokens,
        &mut assembly.remaining,
        &mut assembly.truncated,
    );
    Ok(())
}

fn truncate_into<T>(
    destination: &mut Vec<T>,
    source: &mut Vec<T>,
    remaining: &mut usize,
    truncated: &mut bool,
) {
    if source.len() > *remaining {
        source.truncate(*remaining);
        *truncated = true;
    }
    *remaining = remaining.saturating_sub(source.len());
    destination.append(source);
}

fn configured_account(
    config: &PulseConfig,
    account_id: AccountId,
) -> PulseResult<&super::PulseAccountConfig> {
    config
        .accounts
        .iter()
        .find(|account| account.id == account_id.get())
        .ok_or_else(|| {
            PulseError::new(
                PulseErrorKind::NotFound,
                "Pulse account is not explicitly configured",
            )
        })
}

fn validate_reporter_presence(
    config: &PulseConfig,
    reporter: Option<&Arc<PulseReporter>>,
) -> PulseResult<()> {
    if config.report_to.is_some() != reporter.is_some() {
        return Err(PulseError::configuration(
            "Pulse one-shot reporter must exactly match pulse.report_to",
        ));
    }
    Ok(())
}

fn report_token_ref(config: &PulseConfig) -> Option<SecretRef> {
    match (&config.report_token_env, &config.report_token_file) {
        (Some(name), None) => Some(SecretRef::Environment { name: name.clone() }),
        (None, Some(path)) => Some(SecretRef::File { path: path.clone() }),
        _ => None,
    }
}

fn report_node_token_ref(config: &PulseConfig) -> Option<SecretRef> {
    match (
        &config.report_node_token_env,
        &config.report_node_token_file,
    ) {
        (Some(name), None) => Some(SecretRef::Environment { name: name.clone() }),
        (None, Some(path)) => Some(SecretRef::File { path: path.clone() }),
        _ => None,
    }
}

fn secret_health(reference: Option<SecretRef>) -> ExternalSecretHealth {
    match reference {
        None => ExternalSecretHealth::NotConfigured,
        Some(reference) if reference.resolve().is_ok() => ExternalSecretHealth::Available,
        Some(_) => ExternalSecretHealth::Unavailable,
    }
}

fn cancellation_requested(cancellation: &watch::Receiver<bool>) -> bool {
    *cancellation.borrow()
}

async fn run_cancellable<T>(
    future: impl Future<Output = PulseResult<T>>,
    cancellation: &mut watch::Receiver<bool>,
) -> PulseResult<Option<T>> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            changed = cancellation.changed() => {
                if changed.is_err() || *cancellation.borrow() {
                    return Ok(None);
                }
            }
            result = &mut future => return result.map(Some),
        }
    }
}

fn recent_day(now: Instant, lookback_days: u16) -> PulseResult<String> {
    let lookback = i64::from(lookback_days)
        .checked_mul(24 * 60 * 60 * 1_000)
        .ok_or_else(|| PulseError::invalid_input("Pulse token lookback overflowed"))?;
    let since = now
        .epoch_millis()
        .checked_sub(lookback)
        .ok_or_else(|| PulseError::invalid_input("Pulse token lookback underflowed"))?;
    Instant::from_epoch_millis(since)?
        .to_iso8601()
        .get(..10)
        .map(str::to_owned)
        .ok_or_else(|| PulseError::new(PulseErrorKind::Internal, "Pulse date formatting failed"))
}

fn safe_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
}

fn operational_sqlite_path(config: &PulseConfig) -> PulseResult<PathBuf> {
    config.database.sqlite_path.clone().map_or_else(
        || {
            ProjectDirs::from("dev", "ryanmurf", "atmux")
                .map(|directories| directories.data_dir().join("pulse.sqlite3"))
                .ok_or_else(|| {
                    PulseError::configuration("could not determine the Pulse data directory")
                })
        },
        Ok,
    )
}
