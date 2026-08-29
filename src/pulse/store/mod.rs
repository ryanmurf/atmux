//! Typed persistence contract shared by the native Pulse backends.
//!
//! Store futures are `Send` and own their inputs. The `SQLite` implementation
//! moves every database operation to Tokio's blocking pool; callers must not
//! access a `rusqlite::Connection` directly from an async executor.

use std::{collections::BTreeMap, future::Future, pin::Pin};

use serde::{Deserialize, Serialize};

use super::{
    error::{PulseError, PulseErrorKind, PulseResult},
    federation::{
        FederatedRecord, FederationExportPosition, FederationState, LocalFederationRecord,
        OpaqueCursor,
    },
    ingest::{MAX_PUSH_BODY_BYTES, PushEnvelope},
    model::{
        Account, AccountId, AlertSubscription, AlertType, ContextSession, GeminiQuota, Machine,
        MachineName, Percent, Profile, ProfileName, QuotaWindow, TokenGrain, TokenSource,
        UsageContributor, UsageSnapshot, Vendor,
    },
    time::Instant,
    token::{TokenSourceGeneration, TokenTallyCursor},
};

/// Maximum number of durable replay keys retained for one account.
pub const MAX_INGEST_REPLAYS_PER_ACCOUNT: usize = 200_000;
/// Maximum UTF-8 byte length of an operator reply.
pub const MAX_ALERT_REPLY_BYTES: usize = 2_048;
/// Maximum durable operator replies retained for one alert event.
pub const MAX_ALERT_REPLIES_PER_EVENT: usize = 256;
/// Defensive maximum number of pending reset jobs for one account.
pub const MAX_RESET_JOBS_PER_ACCOUNT: usize = 4_096;
/// Reset jobs cannot be scheduled more than 90 days into the future.
pub const MAX_RESET_HORIZON_MILLIS: u64 = 90 * 24 * 60 * 60 * 1_000;
/// Maximum serialized chunks retained for one reporter outbox page.
pub const MAX_REPORTER_PENDING_CHUNKS: usize = 64;
/// Maximum total serialized body bytes retained for one reporter outbox page.
pub const MAX_REPORTER_PENDING_BYTES: usize = 8 * 1024 * 1024;
/// Durable reporter destinations retained across all machines in one account.
pub const MAX_REPORTER_DESTINATIONS_PER_ACCOUNT: usize = 64;

pub mod migrate;
#[cfg(feature = "pulse-postgres")]
pub mod postgres;
pub mod schema;
pub mod sqlite;

#[cfg(feature = "pulse-postgres")]
pub use postgres::PostgresStore;
pub use sqlite::SqliteStore;

/// Boxed future used to keep the store trait object-safe for service adapters.
pub type StoreFuture<T> = Pin<Box<dyn Future<Output = PulseResult<T>> + Send + 'static>>;

/// An append-only snapshot together with its generated storage identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredUsageSnapshot {
    pub id: i64,
    pub snapshot: UsageSnapshot,
}

/// Stable natural-key position for one local token export page.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReporterTokenPosition {
    pub(crate) profile: String,
    pub(crate) session_id: String,
    pub(crate) model: String,
    pub(crate) settings_hash: String,
    pub(crate) day: String,
    pub(crate) source_json: String,
}

impl ReporterTokenPosition {
    /// Derives the SQL ordering key from a validated token grain.
    ///
    /// # Errors
    ///
    /// Returns an internal error if the typed source cannot be serialized.
    pub fn from_grain(grain: &TokenGrain) -> PulseResult<Self> {
        Ok(Self {
            profile: grain.profile.as_str().to_owned(),
            session_id: grain.session_id.as_str().to_owned(),
            model: grain.model.clone(),
            settings_hash: grain.settings_hash.clone(),
            day: grain.day.clone(),
            source_json: serde_json::to_string(&grain.source).map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Internal,
                    "failed to encode Pulse reporter token position",
                )
            })?,
        })
    }
}

/// Durable per-destination progress for bounded local push reporting.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReporterCursorState {
    pub usage_after_id: i64,
    pub token_after: Option<ReporterTokenPosition>,
    pub token_generation: u64,
}

/// Independently resumable reporter stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReporterStreamKind {
    Usage,
    Token,
}

impl ReporterStreamKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Token => "token",
        }
    }
}

/// One exact, secret-free HTTP document retained for crash replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReporterPendingChunk {
    pub request_id: String,
    pub body: Vec<u8>,
    pub rows: usize,
}

/// Proposed outbox page, persisted before its first network send.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReporterPendingDraft {
    pub kind: ReporterStreamKind,
    pub expected: ReporterCursorState,
    pub next: ReporterCursorState,
    pub chunks: Vec<ReporterPendingChunk>,
}

/// Durable outbox page loaded after insertion or process restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReporterPendingPage {
    pub id: i64,
    pub draft: ReporterPendingDraft,
}

impl ReporterPendingDraft {
    pub(crate) fn validate(&self, account_id: AccountId, machine: &MachineName) -> PulseResult<()> {
        validate_reporter_transition(&self.expected, &self.next)?;
        let stream_matches = match self.kind {
            ReporterStreamKind::Usage => {
                self.next.usage_after_id > self.expected.usage_after_id
                    && self.next.token_after == self.expected.token_after
                    && self.next.token_generation == self.expected.token_generation
            }
            ReporterStreamKind::Token => {
                self.next.usage_after_id == self.expected.usage_after_id
                    && self.next.token_after > self.expected.token_after
                    && self.next.token_generation == self.expected.token_generation
            }
        };
        if !stream_matches
            || self.chunks.is_empty()
            || self.chunks.len() > MAX_REPORTER_PENDING_CHUNKS
        {
            return Err(PulseError::invalid_input(
                "Pulse reporter outbox page is invalid",
            ));
        }
        let mut total = 0_usize;
        for chunk in &self.chunks {
            total = total.saturating_add(chunk.body.len());
            if chunk.rows == 0
                || chunk.body.is_empty()
                || chunk.body.len() > MAX_PUSH_BODY_BYTES
                || total > MAX_REPORTER_PENDING_BYTES
            {
                return Err(PulseError::invalid_input(
                    "Pulse reporter outbox page exceeds its bounds",
                ));
            }
            let envelope = serde_json::from_slice::<PushEnvelope>(&chunk.body).map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Storage,
                    "Pulse reporter outbox contains an invalid document",
                )
            })?;
            let canonical = envelope.encode().map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Storage,
                    "Pulse reporter outbox contains an invalid document",
                )
            })?;
            let rows_match = envelope.batch.row_count() == chunk.rows
                && envelope.request_id == chunk.request_id
                && envelope.account_id == Some(account_id)
                && envelope.machine.as_ref() == Some(machine)
                && canonical == chunk.body;
            let kind_matches = match self.kind {
                ReporterStreamKind::Usage => {
                    !envelope.batch.snapshots.is_empty()
                        && envelope.batch.profiles.is_empty()
                        && envelope.batch.token_grains.is_empty()
                        && envelope.batch.context_sessions.is_empty()
                        && envelope.batch.gemini_quotas.is_empty()
                }
                ReporterStreamKind::Token => {
                    envelope.batch.snapshots.is_empty()
                        && envelope.batch.profiles.is_empty()
                        && !envelope.batch.token_grains.is_empty()
                        && envelope.batch.context_sessions.is_empty()
                        && envelope.batch.gemini_quotas.is_empty()
                }
            };
            if !rows_match || !kind_matches {
                return Err(PulseError::new(
                    PulseErrorKind::Storage,
                    "Pulse reporter outbox document does not match its scope",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_reporter_destination(value: &str) -> PulseResult<()> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(PulseError::invalid_input(
            "Pulse reporter destination key is invalid",
        ));
    }
    Ok(())
}

pub(crate) fn validate_reporter_transition(
    expected: &ReporterCursorState,
    next: &ReporterCursorState,
) -> PulseResult<()> {
    if expected.usage_after_id < 0
        || next.usage_after_id < expected.usage_after_id
        || next.token_generation < expected.token_generation
        || (next.token_generation > expected.token_generation
            && expected.token_generation.checked_add(1) != Some(next.token_generation))
    {
        return Err(PulseError::invalid_input(
            "Pulse reporter cursor transition is invalid",
        ));
    }
    if next.token_generation > expected.token_generation {
        if next.usage_after_id != expected.usage_after_id || next.token_after.is_some() {
            return Err(PulseError::invalid_input(
                "Pulse reporter token resync transition is invalid",
            ));
        }
        return Ok(());
    }
    let usage_advanced = next.usage_after_id > expected.usage_after_id;
    match (&expected.token_after, &next.token_after) {
        (Some(_), None) => Err(PulseError::invalid_input(
            "Pulse reporter token cursor cannot reset without a new generation",
        )),
        (expected, next) if expected == next => {
            if usage_advanced {
                Ok(())
            } else {
                Err(PulseError::invalid_input(
                    "Pulse reporter cursor did not advance",
                ))
            }
        }
        (Some(expected), Some(next)) if next > expected && !usage_advanced => Ok(()),
        (None, Some(_)) if !usage_advanced => Ok(()),
        _ => Err(PulseError::invalid_input(
            "Pulse reporter cursor transition advanced incompatible streams",
        )),
    }
}

/// Current account-global value of one quota window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurrentQuotaWindow {
    pub profile: ProfileName,
    pub vendor: Vendor,
    pub window: QuotaWindow,
    pub polled_at: Instant,
    pub contributors: Vec<UsageContributor>,
}

/// One pricing rule. Settings are matched as an ordered subset.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PricingRule {
    pub key: String,
    pub vendor: Vendor,
    pub model_pattern: String,
    pub settings_match: BTreeMap<String, String>,
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
    pub cache_write_5m_per_million_usd: f64,
    pub cache_write_1h_per_million_usd: f64,
    pub cache_read_per_million_usd: f64,
}

impl PricingRule {
    /// Validates stable identifiers and nonnegative finite prices.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for malformed identifiers or rates.
    pub fn validate(&self) -> PulseResult<()> {
        validate_pricing_key(&self.key)?;
        if self.model_pattern.is_empty() || self.model_pattern.len() > 256 {
            return Err(super::error::PulseError::invalid_input(
                "pricing model pattern must be between 1 and 256 bytes",
            ));
        }
        let rates = [
            self.input_per_million_usd,
            self.output_per_million_usd,
            self.cache_write_5m_per_million_usd,
            self.cache_write_1h_per_million_usd,
            self.cache_read_per_million_usd,
        ];
        if rates
            .into_iter()
            .any(|rate| !rate.is_finite() || rate < 0.0)
        {
            return Err(super::error::PulseError::invalid_input(
                "pricing rates must be finite and nonnegative",
            ));
        }
        Ok(())
    }
}

pub(crate) fn validate_pricing_key(key: &str) -> PulseResult<()> {
    if key.is_empty()
        || key.len() > 128
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(super::error::PulseError::invalid_input(
            "pricing key must be a stable identifier",
        ));
    }
    Ok(())
}

/// Persisted alert subscription identity and creation time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredAlertSubscription {
    pub id: i64,
    pub subscription: AlertSubscription,
    pub created_at: Instant,
}

/// Candidate alert event. Cooldown enforcement happens transactionally.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlertEventInput {
    pub account_id: AccountId,
    pub subscription_id: i64,
    pub profile: ProfileName,
    pub alert_type: AlertType,
    pub message: String,
    pub current_value: Option<Percent>,
    pub threshold: Option<Percent>,
    pub triggered_at: Instant,
}

/// Persisted alert event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlertEvent {
    pub id: i64,
    pub input: AlertEventInput,
    pub acknowledged: bool,
}

/// Account-scoped operator response to a durable alert event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertReply {
    pub id: i64,
    pub account_id: AccountId,
    pub event_id: i64,
    pub message: String,
    pub replied_at: Instant,
}

/// Input for atomically acknowledging and replying to an alert.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertReplyInput {
    pub account_id: AccountId,
    pub event_id: i64,
    pub message: String,
    pub replied_at: Instant,
}

/// A rate-limit reset that should resume collection one minute after reset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetResumeInput {
    pub account_id: AccountId,
    pub profile: ProfileName,
    pub resets_at: Instant,
    pub scheduled_at: Instant,
}

/// Durable reset-resume job state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetResumeJob {
    pub id: i64,
    pub input: ResetResumeInput,
    pub resume_at: Instant,
    pub lease_until: Option<Instant>,
    pub attempts: u32,
    pub delivered_at: Option<Instant>,
    pub cancelled_at: Option<Instant>,
}

/// Transactional bounds for durable reset scheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetResumeLimits {
    pub max_pending_per_account: usize,
    pub max_horizon_millis: u64,
}

impl Default for ResetResumeLimits {
    fn default() -> Self {
        Self {
            max_pending_per_account: 512,
            max_horizon_millis: 31 * 24 * 60 * 60 * 1_000,
        }
    }
}

/// Hashed ingest token metadata. Plaintext tokens are never accepted here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestToken {
    pub id: i64,
    pub account_id: AccountId,
    pub machine: MachineName,
    pub token_hash: String,
    pub created_at: Instant,
    pub last_used_at: Option<Instant>,
    pub revoked_at: Option<Instant>,
}

/// Natural key recording an imported source row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportProvenance {
    pub account_id: AccountId,
    pub source_fingerprint: String,
    pub source_table: String,
    pub source_row_id: String,
    pub target_key: String,
    pub payload_fingerprint: String,
    pub imported_at: Instant,
}

/// One imported value and the logical provenance that guards its write.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedRow<T> {
    pub provenance: ImportProvenance,
    pub value: T,
}

/// Legacy alert event plus the subscription natural key used to remap its
/// source-local subscription id without preserving a global database id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImportedAlertEvent {
    pub subscription: AlertSubscription,
    pub input: AlertEventInput,
    pub acknowledged: bool,
}

/// Legacy alert subscription without its source-local database id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImportedAlertSubscription {
    pub subscription: AlertSubscription,
    pub created_at: Instant,
}

/// Maximum rows written by one atomic import transaction.
pub const MAX_IMPORT_BATCH_ROWS: usize = 5_000;
/// Maximum distinct profile/day totals reconciled by one import.
pub const MAX_IMPORT_RECONCILIATION_KEYS: usize = 10_000;

/// One account-scoped token aggregation key used by import reconciliation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TokenReconciliationKey {
    pub profile: ProfileName,
    pub day: String,
}

/// Exact nonnegative token sums for one reconciliation key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoredTokenTotals {
    pub tokens_in: u128,
    pub tokens_out: u128,
    pub cache_write_5m: u128,
    pub cache_write_1h: u128,
    pub cache_read: u128,
}

/// Maximum local token rows committed with one durable backfill cursor step.
pub const MAX_TOKEN_BACKFILL_PAGE_ROWS: usize = 5_000;

/// Durable progress for one explicit account/profile/machine backfill run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBackfillState {
    pub account_id: AccountId,
    pub profile: ProfileName,
    pub machine: MachineName,
    pub generation: u64,
    pub source_generation: TokenSourceGeneration,
    pub write_revision: u64,
    pub cursor: Option<TokenTallyCursor>,
    pub complete: bool,
}

/// One compare-and-swap token page whose rows and next cursor commit together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenBackfillPage {
    pub expected: TokenBackfillState,
    pub rows: Vec<TokenGrain>,
    pub next_cursor: Option<TokenTallyCursor>,
    pub complete: bool,
}

/// Durable ordering witness allocated before a local token source is scanned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenWriteObservation {
    account_id: AccountId,
    profile: ProfileName,
    machine: MachineName,
    revision: u64,
}

impl TokenWriteObservation {
    pub(crate) fn validate(&self) -> PulseResult<()> {
        if self.revision == 0 {
            return Err(PulseError::invalid_input(
                "Pulse token observation revision is invalid",
            ));
        }
        Ok(())
    }
}

impl TokenBackfillState {
    pub(crate) fn validate(&self) -> PulseResult<()> {
        if self.generation == 0 {
            return Err(PulseError::invalid_input(
                "Pulse token backfill generation is invalid",
            ));
        }
        self.source_generation.validate()?;
        if let Some(cursor) = &self.cursor {
            cursor.validate()?;
        }
        Ok(())
    }
}

impl TokenBackfillPage {
    pub(crate) fn validate(&self) -> PulseResult<()> {
        self.expected.validate()?;
        if self.expected.complete
            || self.rows.len() > MAX_TOKEN_BACKFILL_PAGE_ROWS
            || (self.rows.is_empty() && !self.complete)
        {
            return Err(PulseError::invalid_input(
                "Pulse token backfill page is invalid",
            ));
        }
        let mut last = self.expected.cursor.clone();
        for row in &self.rows {
            row.validate()?;
            if row.account_id != self.expected.account_id
                || row.profile != self.expected.profile
                || row.machine != self.expected.machine
                || row.source != TokenSource::Local
            {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse token backfill row is outside its account scope",
                ));
            }
            let cursor = TokenTallyCursor::from_grain(row);
            if last.as_ref().is_some_and(|prior| cursor <= *prior) {
                return Err(PulseError::invalid_input(
                    "Pulse token backfill rows are not strictly ordered",
                ));
            }
            last = Some(cursor);
        }
        if self.next_cursor != last || (!self.complete && self.next_cursor.is_none()) {
            return Err(PulseError::invalid_input(
                "Pulse token backfill next cursor does not match its rows",
            ));
        }
        Ok(())
    }
}

/// A bounded import transaction. Prerequisite machines are idempotent support
/// rows; every other value is written only when its provenance is first seen.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportBatch {
    pub account_id: AccountId,
    pub prerequisite_machines: Vec<Machine>,
    pub profiles: Vec<ImportedRow<Profile>>,
    pub machines: Vec<ImportedRow<Machine>>,
    pub snapshots: Vec<ImportedRow<UsageSnapshot>>,
    pub token_grains: Vec<ImportedRow<TokenGrain>>,
    pub context_sessions: Vec<ImportedRow<ContextSession>>,
    pub gemini_quotas: Vec<ImportedRow<GeminiQuota>>,
    pub pricing_overrides: Vec<ImportedRow<PricingRule>>,
    pub alert_subscriptions: Vec<ImportedRow<ImportedAlertSubscription>>,
    pub alert_events: Vec<ImportedRow<ImportedAlertEvent>>,
}

impl ImportBatch {
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.prerequisite_machines
            .len()
            .saturating_add(self.profiles.len())
            .saturating_add(self.machines.len())
            .saturating_add(self.snapshots.len())
            .saturating_add(self.token_grains.len())
            .saturating_add(self.context_sessions.len())
            .saturating_add(self.gemini_quotas.len())
            .saturating_add(self.pricing_overrides.len())
            .saturating_add(self.alert_subscriptions.len())
            .saturating_add(self.alert_events.len())
    }
}

/// Per-input provenance decisions returned after an atomic import commit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportBatchResult {
    pub profiles: Vec<bool>,
    pub machines: Vec<bool>,
    pub snapshots: Vec<bool>,
    pub token_grains: Vec<bool>,
    pub context_sessions: Vec<bool>,
    pub gemini_quotas: Vec<bool>,
    pub pricing_overrides: Vec<bool>,
    pub alert_subscriptions: Vec<bool>,
    pub alert_events: Vec<bool>,
}

/// Hard per-account bounds applied to one ingest transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IngestLimits {
    pub max_rows_per_request: usize,
    pub max_profiles: usize,
    pub max_usage_snapshots: usize,
    pub max_token_rows: usize,
    pub max_context_sessions: usize,
    pub max_gemini_models: usize,
}

impl Default for IngestLimits {
    fn default() -> Self {
        Self {
            max_rows_per_request: 5_000,
            max_profiles: 256,
            max_usage_snapshots: 200_000,
            max_token_rows: 200_000,
            max_context_sessions: 5_000,
            max_gemini_models: 512,
        }
    }
}

/// All rows in one authenticated ingest request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IngestBatch {
    pub profiles: Vec<Profile>,
    pub snapshots: Vec<UsageSnapshot>,
    pub token_grains: Vec<TokenGrain>,
    pub context_sessions: Vec<ContextSession>,
    pub gemini_quotas: Vec<GeminiQuota>,
}

impl IngestBatch {
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.snapshots
            .len()
            .saturating_add(self.profiles.len())
            .saturating_add(self.token_grains.len())
            .saturating_add(self.context_sessions.len())
            .saturating_add(self.gemini_quotas.len())
    }
}

/// Counts written by a successful ingest transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngestResult {
    pub snapshots: usize,
    pub token_grains: usize,
    pub context_sessions: usize,
    pub gemini_quotas: usize,
}

/// Safe idempotency key for one authenticated ingest request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestReplay {
    pub request_id: String,
    pub payload_fingerprint: String,
    pub received_at: Instant,
}

/// Result of a transactional idempotent ingest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdempotentIngestResult {
    pub result: IngestResult,
    pub replayed: bool,
}

/// Retention work completed in one transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetentionResult {
    pub context_sessions: usize,
    pub usage_windows: usize,
    pub usage_snapshots: usize,
    pub alert_events: usize,
}

/// Typed persistence boundary used by REST, MCP, federation, and schedulers.
pub trait Store: Send + Sync {
    fn schema_version(&self) -> StoreFuture<u32>;
    fn integrity_check(&self) -> StoreFuture<String>;

    fn upsert_account(&self, account: Account) -> StoreFuture<()>;
    fn get_account(&self, account_id: AccountId) -> StoreFuture<Option<Account>>;

    fn upsert_machine(&self, machine: Machine) -> StoreFuture<()>;
    fn list_machines(&self, account_id: AccountId) -> StoreFuture<Vec<Machine>>;

    fn upsert_profile(&self, profile: Profile) -> StoreFuture<()>;
    fn get_profile(&self, account_id: AccountId, name: ProfileName)
    -> StoreFuture<Option<Profile>>;
    fn list_profiles(&self, account_id: AccountId) -> StoreFuture<Vec<Profile>>;
    fn set_profile_hidden(
        &self,
        account_id: AccountId,
        name: ProfileName,
        hidden: bool,
    ) -> StoreFuture<bool>;
    fn delete_profile(&self, account_id: AccountId, name: ProfileName) -> StoreFuture<bool>;

    fn append_usage_snapshot(&self, snapshot: UsageSnapshot) -> StoreFuture<i64>;
    fn usage_history(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        since: Option<Instant>,
        limit: usize,
    ) -> StoreFuture<Vec<StoredUsageSnapshot>>;
    fn current_usage(
        &self,
        account_id: AccountId,
        profile: ProfileName,
    ) -> StoreFuture<Vec<CurrentQuotaWindow>>;

    fn upsert_context_session(&self, session: ContextSession) -> StoreFuture<()>;
    fn list_context_sessions(
        &self,
        account_id: AccountId,
        profile: Option<ProfileName>,
    ) -> StoreFuture<Vec<ContextSession>>;
    fn upsert_token_grain(&self, grain: TokenGrain) -> StoreFuture<()>;
    fn begin_token_observation(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        machine: MachineName,
    ) -> StoreFuture<TokenWriteObservation>;
    fn upsert_observed_token_grain(
        &self,
        observation: TokenWriteObservation,
        grain: TokenGrain,
    ) -> StoreFuture<()>;
    fn list_token_grains(
        &self,
        account_id: AccountId,
        profile: Option<ProfileName>,
        since_day: Option<String>,
        limit: usize,
    ) -> StoreFuture<Vec<TokenGrain>>;
    fn token_totals_by_keys(
        &self,
        account_id: AccountId,
        keys: Vec<TokenReconciliationKey>,
    ) -> StoreFuture<Vec<(TokenReconciliationKey, StoredTokenTotals)>>;

    fn upsert_pricing_default(&self, rule: PricingRule) -> StoreFuture<()>;
    fn upsert_pricing_override(&self, account_id: AccountId, rule: PricingRule) -> StoreFuture<()>;
    fn delete_pricing_override(&self, account_id: AccountId, key: String) -> StoreFuture<bool>;
    fn list_pricing_defaults(&self) -> StoreFuture<Vec<PricingRule>>;
    fn list_pricing_overrides(&self, account_id: AccountId) -> StoreFuture<Vec<PricingRule>>;

    fn create_alert_subscription(
        &self,
        subscription: AlertSubscription,
        created_at: Instant,
    ) -> StoreFuture<StoredAlertSubscription>;
    fn list_alert_subscriptions(
        &self,
        account_id: AccountId,
    ) -> StoreFuture<Vec<StoredAlertSubscription>>;
    fn delete_alert_subscription(
        &self,
        account_id: AccountId,
        subscription_id: i64,
    ) -> StoreFuture<bool>;
    fn record_alert_if_due(&self, event: AlertEventInput) -> StoreFuture<Option<AlertEvent>>;
    fn list_alert_events(
        &self,
        account_id: AccountId,
        acknowledged: Option<bool>,
    ) -> StoreFuture<Vec<AlertEvent>>;
    fn acknowledge_alert(&self, account_id: AccountId, event_id: i64) -> StoreFuture<bool>;
    fn reply_to_alert(&self, reply: AlertReplyInput) -> StoreFuture<Option<AlertReply>>;
    fn list_alert_replies(
        &self,
        account_id: AccountId,
        event_id: i64,
    ) -> StoreFuture<Vec<AlertReply>>;

    fn schedule_reset_resume(
        &self,
        input: ResetResumeInput,
        limits: ResetResumeLimits,
    ) -> StoreFuture<ResetResumeJob>;
    fn list_pending_reset_resumes(
        &self,
        account_id: AccountId,
        through: Instant,
        limit: usize,
    ) -> StoreFuture<Vec<ResetResumeJob>>;
    fn claim_due_reset_resumes(
        &self,
        account_id: AccountId,
        now: Instant,
        lease_until: Instant,
        limit: usize,
    ) -> StoreFuture<Vec<ResetResumeJob>>;
    fn complete_reset_resume(
        &self,
        account_id: AccountId,
        job_id: i64,
        delivered_at: Instant,
    ) -> StoreFuture<bool>;
    fn cancel_reset_resumes(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        cancelled_at: Instant,
    ) -> StoreFuture<usize>;

    fn insert_ingest_token(&self, token: IngestToken) -> StoreFuture<()>;
    /// Registers the token's validated machine and inserts the hashed token in
    /// one account-scoped transaction after enforcing the active-token cap.
    /// Implementations must serialize competing issuers for the same account.
    fn issue_ingest_token(
        &self,
        machine: Machine,
        token: IngestToken,
        max_active_tokens: usize,
    ) -> StoreFuture<()>;
    fn list_ingest_tokens(&self, account_id: AccountId) -> StoreFuture<Vec<IngestToken>>;
    fn get_ingest_token(
        &self,
        account_id: AccountId,
        token_id: i64,
    ) -> StoreFuture<Option<IngestToken>>;
    fn touch_ingest_token(
        &self,
        account_id: AccountId,
        token_id: i64,
        used_at: Instant,
    ) -> StoreFuture<bool>;
    fn revoke_ingest_token(
        &self,
        account_id: AccountId,
        token_id: i64,
        revoked_at: Instant,
    ) -> StoreFuture<bool>;

    fn upsert_gemini_quota(&self, quota: GeminiQuota) -> StoreFuture<()>;
    fn list_gemini_quotas(&self, account_id: AccountId) -> StoreFuture<Vec<GeminiQuota>>;
    fn record_import(&self, provenance: ImportProvenance) -> StoreFuture<bool>;
    fn append_imported_usage_snapshot_once(
        &self,
        provenance: ImportProvenance,
        snapshot: UsageSnapshot,
    ) -> StoreFuture<bool>;
    fn apply_import_batch_once(&self, batch: ImportBatch) -> StoreFuture<ImportBatchResult>;

    /// Begins or resumes an explicit full-history token run. A changed source
    /// restarts from the beginning; a completed run restarts only explicitly.
    fn begin_token_backfill(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        machine: MachineName,
        source_generation: TokenSourceGeneration,
        restart_completed: bool,
    ) -> StoreFuture<TokenBackfillState>;
    /// Atomically upserts one bounded page and compare-and-swaps its cursor.
    fn apply_token_backfill_page(&self, page: TokenBackfillPage)
    -> StoreFuture<TokenBackfillState>;

    /// Loads a peer's cursor or starts the next full resync generation after
    /// a completed scan.
    fn begin_federation_sync(
        &self,
        account_id: AccountId,
        source_machine: MachineName,
    ) -> StoreFuture<FederationState>;
    /// Atomically applies an authenticated page and advances its cursor.
    fn apply_federation_page(
        &self,
        account_id: AccountId,
        source_machine: MachineName,
        expected_cursor: Option<OpaqueCursor>,
        next_cursor: Option<OpaqueCursor>,
        records: Vec<FederatedRecord>,
    ) -> StoreFuture<FederationState>;
    /// Reads one keyset page of rows produced by exactly this local machine.
    /// Implementations must apply origin/machine predicates before the bound.
    fn local_federation_page(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        after: Option<FederationExportPosition>,
        limit: usize,
    ) -> StoreFuture<Vec<LocalFederationRecord>>;

    /// Loads or creates durable reporting progress for one secret-free
    /// destination identity.
    fn load_reporter_cursor(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
    ) -> StoreFuture<ReporterCursorState>;
    /// Reads append-only local usage directly by stable generated id.
    fn local_reporter_usage_page(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        after_id: i64,
        limit: usize,
    ) -> StoreFuture<Vec<StoredUsageSnapshot>>;
    /// Reads mutable local token rows directly by their stable natural key.
    fn local_reporter_token_page(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        after: Option<ReporterTokenPosition>,
        limit: usize,
    ) -> StoreFuture<Vec<TokenGrain>>;
    /// Compare-and-swaps durable progress after a complete remote page has
    /// succeeded. A stale writer fails without changing the cursor.
    fn advance_reporter_cursor(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        expected: ReporterCursorState,
        next: ReporterCursorState,
    ) -> StoreFuture<ReporterCursorState>;
    /// Loads an exact pending page prepared before its first network send.
    fn load_reporter_pending(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        kind: ReporterStreamKind,
    ) -> StoreFuture<Option<ReporterPendingPage>>;
    /// Creates one bounded outbox page, or returns the already durable page for
    /// this stream when another process/restart prepared it first.
    fn prepare_reporter_pending(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        draft: ReporterPendingDraft,
    ) -> StoreFuture<ReporterPendingPage>;
    /// Atomically applies the pending page's intended cursor transition and
    /// deletes its exact serialized chunks after every remote chunk succeeds.
    fn commit_reporter_pending(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        kind: ReporterStreamKind,
        pending_id: i64,
    ) -> StoreFuture<ReporterCursorState>;

    fn ingest_batch(
        &self,
        account_id: AccountId,
        machine: MachineName,
        batch: IngestBatch,
        limits: IngestLimits,
    ) -> StoreFuture<IngestResult>;
    fn ingest_batch_once(
        &self,
        account_id: AccountId,
        machine: MachineName,
        batch: IngestBatch,
        limits: IngestLimits,
        replay: IngestReplay,
    ) -> StoreFuture<IdempotentIngestResult>;
    fn apply_retention(
        &self,
        now: Instant,
        context_days: u16,
        alert_days: u16,
        hourly_after_days: u16,
        daily_after_days: u16,
    ) -> StoreFuture<RetentionResult>;
}
