//! Bounded, read-only import of the legacy Claude Pulse `SQLite` database.
//!
//! The importer deliberately projects only rows that have a lossless native
//! representation. Legacy inline API keys, raw provider responses, and ingest
//! token hashes are never selected from the source database.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, Metadata},
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

use rusqlite::{Connection, OpenFlags, Row, params_from_iter};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::pulse::{
    AccountId, AgentSettings, AlertSubscription, AlertType, CollectionOutcome, ContextSession,
    Fraction, GeminiQuota, Instant, Machine, MachineName, Percent, Profile, ProfileName,
    ProfileOrigin, QuotaWindow, QuotaWindowKind, RefreshPolicy, SessionId, TokenGrain, TokenSource,
    UsageSnapshot, Vendor,
    error::{PulseError, PulseErrorKind, PulseResult},
    store::{
        AlertEventInput, ImportBatch, ImportProvenance, ImportedAlertEvent,
        ImportedAlertSubscription, ImportedRow, MAX_IMPORT_BATCH_ROWS, PricingRule, Store,
        StoredTokenTotals, TokenReconciliationKey,
    },
};

const MAX_HARD_SOURCE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_HARD_TABLES: usize = 256;
const MAX_HARD_ROWS: usize = 1_000_000;
const MAX_HARD_TEXT_BYTES: usize = 1024 * 1024;
const MAX_HARD_TOTAL_TEXT_BYTES: usize = 64 * 1024 * 1024;
const MAX_MACHINE_ALIASES: usize = 64;

const SUPPORTED_TABLES: &[&str] = &[
    "profiles",
    "machines",
    "usage_snapshots",
    "token_usage",
    "context_sessions",
    "gemini_quota",
    "pricing_overrides",
    "alert_subscriptions",
    "alert_events",
];

const KNOWN_EXCLUDED_TABLES: &[(&str, ImportDiagnosticCode)] = &[
    ("ingest_tokens", ImportDiagnosticCode::IngestTokensExcluded),
    (
        "token_rollups",
        ImportDiagnosticCode::LossyLegacyRollupExcluded,
    ),
    (
        "pricing_defaults",
        ImportDiagnosticCode::AuthoritativePricingDefaultsSeeded,
    ),
];

/// Resource limits applied before any source rows are materialized.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportLimits {
    pub max_source_bytes: u64,
    pub max_tables: usize,
    pub max_rows_per_table: usize,
    pub max_total_rows: usize,
    pub max_text_bytes: usize,
    pub max_total_text_bytes: usize,
    pub max_diagnostics: usize,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 768 * 1024 * 1024,
            max_tables: 64,
            max_rows_per_table: 500_000,
            max_total_rows: 600_000,
            max_text_bytes: 256 * 1024,
            max_total_text_bytes: 32 * 1024 * 1024,
            max_diagnostics: 2_048,
        }
    }
}

impl ImportLimits {
    fn validate(self) -> PulseResult<()> {
        let valid = self.max_source_bytes > 0
            && self.max_source_bytes <= MAX_HARD_SOURCE_BYTES
            && self.max_tables > 0
            && self.max_tables <= MAX_HARD_TABLES
            && self.max_rows_per_table > 0
            && self.max_rows_per_table <= MAX_HARD_ROWS
            && self.max_total_rows > 0
            && self.max_total_rows <= MAX_HARD_ROWS
            && self.max_text_bytes > 0
            && self.max_text_bytes <= MAX_HARD_TEXT_BYTES
            && self.max_total_text_bytes > 0
            && self.max_total_text_bytes <= MAX_HARD_TOTAL_TEXT_BYTES
            && self.max_diagnostics > 0
            && self.max_diagnostics <= MAX_HARD_ROWS;
        if !valid || self.max_rows_per_table > self.max_total_rows {
            return Err(PulseError::invalid_input(
                "Pulse import limits are outside the supported bounds",
            ));
        }
        Ok(())
    }
}

/// An external credential reference replacing a legacy inline API key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ExternalCredential {
    Environment(String),
    File(PathBuf),
}

/// Inputs for one non-destructive legacy import.
#[derive(Clone, Debug)]
pub struct ImportRequest {
    pub source: PathBuf,
    pub target_account_id: AccountId,
    /// Required when the source contains more than one legacy account.
    pub source_account_id: Option<i64>,
    /// Explicit attribution for old snapshot rows whose `machine` is absent.
    pub fallback_machine: Option<MachineName>,
    /// Explicit legacy-to-canonical machine mappings for the selected account.
    pub machine_aliases: BTreeMap<MachineName, MachineName>,
    /// Explicit replacements for legacy inline profile secrets.
    pub credentials: BTreeMap<ProfileName, ExternalCredential>,
    /// Target-side operational policy; the legacy schema did not type it.
    pub refresh: RefreshPolicy,
    pub imported_at: Instant,
    pub dry_run: bool,
    pub limits: ImportLimits,
}

impl ImportRequest {
    /// Constructs a request with bounded defaults and no implicit source
    /// account, machine, or credential mapping.
    #[must_use]
    pub fn new(
        source: PathBuf,
        target_account_id: AccountId,
        refresh: RefreshPolicy,
        imported_at: Instant,
    ) -> Self {
        Self {
            source,
            target_account_id,
            source_account_id: None,
            fallback_machine: None,
            machine_aliases: BTreeMap::new(),
            credentials: BTreeMap::new(),
            refresh,
            imported_at,
            dry_run: false,
            limits: ImportLimits::default(),
        }
    }
}

fn validate_machine_aliases(request: &ImportRequest) -> PulseResult<()> {
    if request.machine_aliases.len() > MAX_MACHINE_ALIASES {
        return Err(PulseError::invalid_input(
            "Pulse import machine alias count exceeds its bound",
        ));
    }
    for (source, target) in &request.machine_aliases {
        if source == target || request.machine_aliases.contains_key(target) {
            return Err(PulseError::invalid_input(
                "Pulse import machine aliases must map directly to distinct canonical names",
            ));
        }
    }
    Ok(())
}

/// Stable machine-readable reasons for skipped or transformed source data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportDiagnosticCode {
    MissingTable,
    MissingColumn,
    SourceAccountColumnAbsent,
    SourceAccountRowUnscoped,
    InlineSecretExternalized,
    InlineSecretExcluded,
    RawProviderResponseExcluded,
    IngestTokensExcluded,
    LossyLegacyRollupExcluded,
    UnsupportedTable,
    AuthoritativePricingDefaultsSeeded,
    AlertDeliveryExcluded,
    CredentialReferenceRequired,
    ExplicitMachineRequired,
    MissingDependency,
    InvalidSourceRow,
    UnrepresentableSourceRow,
    DiagnosticsTruncated,
    TargetReconciliationBoundReached,
    LegacyProfileVisibilityDefaulted,
    MachineAliasApplied,
}

/// A secret-free typed import diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportDiagnostic {
    pub code: ImportDiagnosticCode,
    pub table: Option<String>,
    pub source_row_id: Option<String>,
    pub column: Option<String>,
    pub message: String,
}

/// Per-table progress. `replayed` means provenance already existed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableImportStats {
    pub discovered: usize,
    pub planned: usize,
    pub imported: usize,
    pub replayed: usize,
    pub skipped: usize,
}

/// Exact token totals used for source/target reconciliation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub tokens_in: u128,
    pub tokens_out: u128,
    pub cache_write_5m: u128,
    pub cache_write_1h: u128,
    pub cache_read: u128,
}

impl TokenTotals {
    fn add(&mut self, grain: &TokenGrain) {
        self.tokens_in += u128::from(grain.tokens_in);
        self.tokens_out += u128::from(grain.tokens_out);
        self.cache_write_5m += u128::from(grain.cache_write_5m);
        self.cache_write_1h += u128::from(grain.cache_write_1h);
        self.cache_read += u128::from(grain.cache_read);
    }
}

/// One exact per-profile/day comparison.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationRow {
    pub profile: ProfileName,
    pub day: String,
    pub source: TokenTotals,
    pub target: TokenTotals,
    pub exact: bool,
}

/// Result of a dry-run or executed import.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    pub source_fingerprint: String,
    pub target_account_id: AccountId,
    pub selected_source_account_id: Option<i64>,
    pub dry_run: bool,
    pub tables: BTreeMap<String, TableImportStats>,
    pub diagnostics: Vec<ImportDiagnostic>,
    pub reconciliation: Vec<ReconciliationRow>,
    pub reconciliation_complete: bool,
    pub reconciliation_exact: bool,
}

#[derive(Clone, Debug)]
struct Planned<T> {
    source_table: &'static str,
    source_row_id: String,
    target_key: String,
    payload_fingerprint: String,
    value: T,
}

impl<T: Serialize> Planned<T> {
    fn new(
        source_table: &'static str,
        source_row_id: String,
        target_key: String,
        value: T,
    ) -> PulseResult<Self> {
        let payload_fingerprint = canonical_payload_fingerprint(&value)?;
        Ok(Self {
            source_table,
            source_row_id,
            target_key,
            payload_fingerprint,
            value,
        })
    }
}

#[derive(Debug)]
struct ImportPlan {
    fingerprint: String,
    target_account_id: AccountId,
    selected_source_account_id: Option<i64>,
    profiles: Vec<Planned<Profile>>,
    source_machines: Vec<Planned<Machine>>,
    prerequisite_machines: BTreeMap<MachineName, Machine>,
    snapshots: Vec<Planned<UsageSnapshot>>,
    tokens: Vec<Planned<TokenGrain>>,
    contexts: Vec<Planned<ContextSession>>,
    gemini: Vec<Planned<GeminiQuota>>,
    pricing_overrides: Vec<Planned<PricingRule>>,
    alert_subscriptions: Vec<Planned<ImportedAlertSubscription>>,
    alert_events: Vec<Planned<ImportedAlertEvent>>,
    tables: BTreeMap<String, TableImportStats>,
    diagnostics: Vec<ImportDiagnostic>,
    diagnostics_truncated: bool,
    machine_alias_counts: BTreeMap<MachineName, usize>,
}

impl ImportPlan {
    fn diagnostic(
        &mut self,
        limits: ImportLimits,
        code: ImportDiagnosticCode,
        table: Option<&str>,
        source_row_id: Option<String>,
        column: Option<&str>,
        message: impl Into<String>,
    ) {
        if self.diagnostics.len() < limits.max_diagnostics {
            self.diagnostics.push(ImportDiagnostic {
                code,
                table: table.map(str::to_owned),
                source_row_id,
                column: column.map(str::to_owned),
                message: message.into(),
            });
        } else {
            self.diagnostics_truncated = true;
        }
    }

    fn finish_diagnostics(&mut self, limits: ImportLimits) {
        if !self.diagnostics_truncated {
            return;
        }
        if self.diagnostics.len() == limits.max_diagnostics {
            self.diagnostics.pop();
        }
        self.diagnostics.push(ImportDiagnostic {
            code: ImportDiagnosticCode::DiagnosticsTruncated,
            table: None,
            source_row_id: None,
            column: None,
            message: "additional bounded import diagnostics were omitted".to_owned(),
        });
    }

    fn skip(&mut self, table: &str) {
        self.tables.entry(table.to_owned()).or_default().skipped += 1;
    }

    fn planned(&mut self, table: &str) {
        self.tables.entry(table.to_owned()).or_default().planned += 1;
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    length: u64,
}

#[derive(Debug)]
struct PathWitness {
    path: PathBuf,
    identity: FileIdentity,
    is_directory: bool,
}

#[cfg(unix)]
fn file_identity(metadata: &Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn file_identity(metadata: &Metadata) -> FileIdentity {
    FileIdentity {
        length: metadata.len(),
    }
}

/// Imports a legacy Claude Pulse database without opening the source writable.
///
/// # Errors
///
/// Returns a classified error for an unsafe source path, bounds violation,
/// malformed database, missing target account, or failed target write.
pub async fn import_legacy_sqlite(
    store: &dyn Store,
    request: ImportRequest,
) -> PulseResult<ImportReport> {
    request.limits.validate()?;
    validate_machine_aliases(&request)?;
    if request.source_account_id.is_some_and(|id| id <= 0) {
        return Err(PulseError::invalid_input(
            "legacy source account id must be positive",
        ));
    }

    let target_account_id = request.target_account_id;
    if store.get_account(target_account_id).await?.is_none() {
        return Err(PulseError::new(
            PulseErrorKind::NotFound,
            "the explicit Pulse import target account does not exist",
        ));
    }

    let inspect_request = request.clone();
    let mut plan = tokio::task::spawn_blocking(move || inspect_source(&inspect_request))
        .await
        .map_err(|_| {
            PulseError::new(
                PulseErrorKind::Internal,
                "the bounded Pulse source inspection task failed",
            )
        })??;

    if !request.dry_run {
        execute_plan(store, &request, &mut plan).await?;
    }
    let (reconciliation, complete) = reconcile_tokens(store, &plan, request.dry_run).await?;
    if !complete {
        plan.diagnostic(
            request.limits,
            ImportDiagnosticCode::TargetReconciliationBoundReached,
            Some("token_usage"),
            None,
            None,
            "target token rows reached the reconciliation query bound",
        );
    }
    plan.finish_diagnostics(request.limits);
    let exact = complete && reconciliation.iter().all(|row| row.exact);

    Ok(ImportReport {
        source_fingerprint: plan.fingerprint,
        target_account_id: plan.target_account_id,
        selected_source_account_id: plan.selected_source_account_id,
        dry_run: request.dry_run,
        tables: plan.tables,
        diagnostics: plan.diagnostics,
        reconciliation,
        reconciliation_complete: complete,
        reconciliation_exact: exact,
    })
}

async fn execute_plan(
    store: &dyn Store,
    request: &ImportRequest,
    plan: &mut ImportPlan,
) -> PulseResult<()> {
    for chunk in plan.profiles.chunks(MAX_IMPORT_BATCH_ROWS) {
        let mut batch = empty_import_batch(request.target_account_id);
        batch.profiles = imported_rows(request, &plan.fingerprint, chunk);
        let decisions = store.apply_import_batch_once(batch).await?.profiles;
        apply_decisions(&mut plan.tables, "profiles", &decisions);
    }
    for chunk in plan.source_machines.chunks(MAX_IMPORT_BATCH_ROWS) {
        let mut batch = empty_import_batch(request.target_account_id);
        batch.machines = imported_rows(request, &plan.fingerprint, chunk);
        let decisions = store.apply_import_batch_once(batch).await?.machines;
        apply_decisions(&mut plan.tables, "machines", &decisions);
    }
    let prerequisite_machines = plan
        .prerequisite_machines
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for chunk in prerequisite_machines.chunks(MAX_IMPORT_BATCH_ROWS) {
        let mut batch = empty_import_batch(request.target_account_id);
        batch.prerequisite_machines = chunk.to_vec();
        store.apply_import_batch_once(batch).await?;
    }
    for chunk in plan.snapshots.chunks(MAX_IMPORT_BATCH_ROWS) {
        let mut batch = empty_import_batch(request.target_account_id);
        batch.snapshots = imported_rows(request, &plan.fingerprint, chunk);
        let decisions = store.apply_import_batch_once(batch).await?.snapshots;
        apply_decisions(&mut plan.tables, "usage_snapshots", &decisions);
    }
    for chunk in plan.tokens.chunks(MAX_IMPORT_BATCH_ROWS) {
        let mut batch = empty_import_batch(request.target_account_id);
        batch.token_grains = imported_rows(request, &plan.fingerprint, chunk);
        let decisions = store.apply_import_batch_once(batch).await?.token_grains;
        apply_decisions(&mut plan.tables, "token_usage", &decisions);
    }
    for chunk in plan.contexts.chunks(MAX_IMPORT_BATCH_ROWS) {
        let mut batch = empty_import_batch(request.target_account_id);
        batch.context_sessions = imported_rows(request, &plan.fingerprint, chunk);
        let decisions = store.apply_import_batch_once(batch).await?.context_sessions;
        apply_decisions(&mut plan.tables, "context_sessions", &decisions);
    }
    for chunk in plan.gemini.chunks(MAX_IMPORT_BATCH_ROWS) {
        let mut batch = empty_import_batch(request.target_account_id);
        batch.gemini_quotas = imported_rows(request, &plan.fingerprint, chunk);
        let decisions = store.apply_import_batch_once(batch).await?.gemini_quotas;
        apply_decisions(&mut plan.tables, "gemini_quota", &decisions);
    }
    for chunk in plan.pricing_overrides.chunks(MAX_IMPORT_BATCH_ROWS) {
        let mut batch = empty_import_batch(request.target_account_id);
        batch.pricing_overrides = imported_rows(request, &plan.fingerprint, chunk);
        let decisions = store
            .apply_import_batch_once(batch)
            .await?
            .pricing_overrides;
        apply_decisions(&mut plan.tables, "pricing_overrides", &decisions);
    }
    for chunk in plan.alert_subscriptions.chunks(MAX_IMPORT_BATCH_ROWS) {
        let mut batch = empty_import_batch(request.target_account_id);
        batch.alert_subscriptions = imported_rows(request, &plan.fingerprint, chunk);
        let decisions = store
            .apply_import_batch_once(batch)
            .await?
            .alert_subscriptions;
        apply_decisions(&mut plan.tables, "alert_subscriptions", &decisions);
    }
    for chunk in plan.alert_events.chunks(MAX_IMPORT_BATCH_ROWS) {
        let mut batch = empty_import_batch(request.target_account_id);
        batch.alert_events = imported_rows(request, &plan.fingerprint, chunk);
        let decisions = store.apply_import_batch_once(batch).await?.alert_events;
        apply_decisions(&mut plan.tables, "alert_events", &decisions);
    }
    Ok(())
}

fn empty_import_batch(account_id: AccountId) -> ImportBatch {
    ImportBatch {
        account_id,
        prerequisite_machines: Vec::new(),
        profiles: Vec::new(),
        machines: Vec::new(),
        snapshots: Vec::new(),
        token_grains: Vec::new(),
        context_sessions: Vec::new(),
        gemini_quotas: Vec::new(),
        pricing_overrides: Vec::new(),
        alert_subscriptions: Vec::new(),
        alert_events: Vec::new(),
    }
}

fn imported_rows<T: Clone>(
    request: &ImportRequest,
    fingerprint: &str,
    rows: &[Planned<T>],
) -> Vec<ImportedRow<T>> {
    rows.iter()
        .map(|row| ImportedRow {
            provenance: provenance(request, fingerprint, row),
            value: row.value.clone(),
        })
        .collect()
}

fn apply_decisions(
    tables: &mut BTreeMap<String, TableImportStats>,
    table: &str,
    decisions: &[bool],
) {
    let stats = tables.entry(table.to_owned()).or_default();
    stats.imported += decisions.iter().filter(|inserted| **inserted).count();
    stats.replayed += decisions.iter().filter(|inserted| !**inserted).count();
}

fn provenance<T>(request: &ImportRequest, fingerprint: &str, row: &Planned<T>) -> ImportProvenance {
    ImportProvenance {
        account_id: request.target_account_id,
        source_fingerprint: fingerprint.to_owned(),
        source_table: row.source_table.to_owned(),
        source_row_id: row.source_row_id.clone(),
        target_key: row.target_key.clone(),
        payload_fingerprint: row.payload_fingerprint.clone(),
        imported_at: request.imported_at,
    }
}

async fn reconcile_tokens(
    store: &dyn Store,
    plan: &ImportPlan,
    dry_run: bool,
) -> PulseResult<(Vec<ReconciliationRow>, bool)> {
    let mut source = BTreeMap::<(ProfileName, String), TokenTotals>::new();
    for row in &plan.tokens {
        source
            .entry((row.value.profile.clone(), row.value.day.clone()))
            .or_default()
            .add(&row.value);
    }
    if source.is_empty() {
        return Ok((Vec::new(), true));
    }

    if dry_run {
        let rows = source
            .into_iter()
            .map(|((profile, day), source)| ReconciliationRow {
                profile,
                day,
                source,
                target: source,
                exact: true,
            })
            .collect();
        return Ok((rows, true));
    }
    let keys = source
        .keys()
        .map(|(profile, day)| TokenReconciliationKey {
            profile: profile.clone(),
            day: day.clone(),
        })
        .collect();
    let mut target = store
        .token_totals_by_keys(plan.target_account_id, keys)
        .await?
        .into_iter()
        .map(|(key, totals)| ((key.profile, key.day), token_totals_from_store(totals)))
        .collect::<BTreeMap<_, _>>();
    let rows = source
        .into_iter()
        .map(|((profile, day), source)| {
            let target = target
                .remove(&(profile.clone(), day.clone()))
                .unwrap_or_default();
            ReconciliationRow {
                profile,
                day,
                source,
                target,
                exact: source == target,
            }
        })
        .collect();
    Ok((rows, true))
}

const fn token_totals_from_store(totals: StoredTokenTotals) -> TokenTotals {
    TokenTotals {
        tokens_in: totals.tokens_in,
        tokens_out: totals.tokens_out,
        cache_write_5m: totals.cache_write_5m,
        cache_write_1h: totals.cache_write_1h,
        cache_read: totals.cache_read,
    }
}

fn inspect_source(request: &ImportRequest) -> PulseResult<ImportPlan> {
    let (canonical, witnesses, mut file, before_digest) =
        secure_source(&request.source, request.limits)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = Connection::open_with_flags(&canonical, flags).map_err(|_| source_error())?;
    connection
        .execute_batch("PRAGMA query_only=ON; BEGIN DEFERRED")
        .map_err(|_| source_error())?;
    verify_opened_main(&connection, &canonical, &witnesses)?;
    let before_data_version = pragma_i64(&connection, "PRAGMA data_version")?;
    let integrity = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0))
        .map_err(|_| source_error())?;
    if integrity != "ok" {
        return Err(PulseError::new(
            PulseErrorKind::Storage,
            "legacy Pulse SQLite integrity check failed",
        ));
    }

    let tables = source_tables(&connection, request.limits)?;
    let selected_source_account_id =
        resolve_source_account(&connection, &tables, request.source_account_id)?;
    let mut plan = new_import_plan(request.target_account_id, selected_source_account_id);

    populate_usage_plan(
        &connection,
        &tables,
        selected_source_account_id,
        request,
        &mut plan,
    )?;
    populate_operator_plan(
        &connection,
        &tables,
        selected_source_account_id,
        request,
        &mut plan,
    )?;

    finish_machine_aliases(request, &mut plan)?;
    normalize_plan_collisions(&mut plan)?;
    validate_plan_identities(&plan)?;
    plan.fingerprint = logical_source_fingerprint(&plan);

    finish_source_inspection(
        &connection,
        before_data_version,
        &witnesses,
        &mut file,
        before_digest,
        request.limits,
    )?;
    plan.finish_diagnostics(request.limits);
    Ok(plan)
}

fn new_import_plan(
    target_account_id: AccountId,
    selected_source_account_id: Option<i64>,
) -> ImportPlan {
    ImportPlan {
        fingerprint: String::new(),
        target_account_id,
        selected_source_account_id,
        profiles: Vec::new(),
        source_machines: Vec::new(),
        prerequisite_machines: BTreeMap::new(),
        snapshots: Vec::new(),
        tokens: Vec::new(),
        contexts: Vec::new(),
        gemini: Vec::new(),
        pricing_overrides: Vec::new(),
        alert_subscriptions: Vec::new(),
        alert_events: Vec::new(),
        tables: BTreeMap::new(),
        diagnostics: Vec::new(),
        diagnostics_truncated: false,
        machine_alias_counts: BTreeMap::new(),
    }
}

fn populate_usage_plan(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    plan: &mut ImportPlan,
) -> PulseResult<()> {
    preflight_tables(connection, tables, source_account_id, request, plan)?;
    let profile_vendors = read_profiles(connection, tables, source_account_id, request, plan)?;
    read_machines(connection, tables, source_account_id, request, plan)?;
    read_snapshots(
        connection,
        tables,
        source_account_id,
        request,
        &profile_vendors,
        plan,
    )?;
    read_tokens(
        connection,
        tables,
        source_account_id,
        request,
        &profile_vendors,
        plan,
    )?;
    read_contexts(
        connection,
        tables,
        source_account_id,
        request,
        &profile_vendors,
        plan,
    )?;
    read_gemini(connection, tables, source_account_id, request, plan)?;
    Ok(())
}

fn populate_operator_plan(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    plan: &mut ImportPlan,
) -> PulseResult<()> {
    let profile_vendors = plan
        .profiles
        .iter()
        .map(|row| (row.value.name.clone(), row.value.vendor))
        .collect::<BTreeMap<_, _>>();
    read_pricing_overrides(connection, tables, source_account_id, request, plan)?;
    let legacy_alert_subscriptions = read_alert_subscriptions(
        connection,
        tables,
        source_account_id,
        request,
        &profile_vendors,
        plan,
    )?;
    read_alert_events(
        connection,
        tables,
        source_account_id,
        request,
        &legacy_alert_subscriptions,
        plan,
    )?;
    Ok(())
}

fn finish_source_inspection(
    connection: &Connection,
    before_data_version: i64,
    witnesses: &[PathWitness],
    file: &mut File,
    before_digest: [u8; 32],
    limits: ImportLimits,
) -> PulseResult<()> {
    let after_data_version = pragma_i64(connection, "PRAGMA data_version")?;
    connection
        .execute_batch("ROLLBACK")
        .map_err(|_| source_error())?;
    if before_data_version != after_data_version {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "legacy Pulse SQLite changed during its read-only import snapshot",
        ));
    }
    verify_witnesses(witnesses)?;
    file.seek(SeekFrom::Start(0)).map_err(|_| source_error())?;
    let after_digest = hash_open_file(file, limits.max_source_bytes)?;
    if before_digest != after_digest {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "legacy Pulse SQLite file changed during import inspection",
        ));
    }
    Ok(())
}

fn secure_source(
    source: &Path,
    limits: ImportLimits,
) -> PulseResult<(PathBuf, Vec<PathWitness>, File, [u8; 32])> {
    if !source.is_absolute() {
        return Err(PulseError::invalid_input(
            "legacy Pulse SQLite source path must be absolute",
        ));
    }
    if source
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(PulseError::invalid_input(
            "legacy Pulse SQLite source path cannot contain dot components",
        ));
    }

    let mut witnesses = Vec::new();
    let ancestors = source.ancestors().collect::<Vec<_>>();
    for path in ancestors.into_iter().rev() {
        let metadata = fs::symlink_metadata(path).map_err(|_| source_error())?;
        if metadata.file_type().is_symlink() {
            return Err(PulseError::invalid_input(
                "legacy Pulse SQLite source and ancestors cannot be symlinks",
            ));
        }
        let is_source = path == source;
        if is_source && !metadata.is_file() {
            return Err(PulseError::invalid_input(
                "legacy Pulse SQLite source must be a regular file",
            ));
        }
        if !is_source && !metadata.is_dir() {
            return Err(PulseError::invalid_input(
                "legacy Pulse SQLite source ancestor must be a directory",
            ));
        }
        if is_source && metadata.len() > limits.max_source_bytes {
            return Err(PulseError::invalid_input(
                "legacy Pulse SQLite source exceeds its byte limit",
            ));
        }
        witnesses.push(PathWitness {
            path: path.to_path_buf(),
            identity: file_identity(&metadata),
            is_directory: metadata.is_dir(),
        });
    }
    let canonical = fs::canonicalize(source).map_err(|_| source_error())?;
    if canonical != source {
        return Err(PulseError::invalid_input(
            "legacy Pulse SQLite source path must already be canonical",
        ));
    }
    let mut file = File::open(&canonical).map_err(|_| source_error())?;
    let metadata = file.metadata().map_err(|_| source_error())?;
    if file_identity(&metadata) != witnesses.last().ok_or_else(source_error)?.identity {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "legacy Pulse SQLite source changed during secure open",
        ));
    }
    let digest = hash_open_file(&mut file, limits.max_source_bytes)?;
    verify_witnesses(&witnesses)?;
    Ok((canonical, witnesses, file, digest))
}

fn verify_witnesses(witnesses: &[PathWitness]) -> PulseResult<()> {
    for witness in witnesses {
        let metadata = fs::symlink_metadata(&witness.path).map_err(|_| source_error())?;
        let valid_type = if witness.is_directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        };
        if metadata.file_type().is_symlink()
            || !valid_type
            || file_identity(&metadata) != witness.identity
        {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "legacy Pulse SQLite source path or ancestor changed during import",
            ));
        }
    }
    Ok(())
}

fn hash_open_file(file: &mut File, max_bytes: u64) -> PulseResult<[u8; 32]> {
    file.seek(SeekFrom::Start(0)).map_err(|_| source_error())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    let mut total = 0_u64;
    loop {
        let count = file.read(&mut buffer).map_err(|_| source_error())?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(u64::try_from(count).map_err(|_| source_error())?)
            .ok_or_else(source_error)?;
        if total > max_bytes {
            return Err(PulseError::invalid_input(
                "legacy Pulse SQLite source exceeds its byte limit",
            ));
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().into())
}

fn logical_source_fingerprint(plan: &ImportPlan) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"claude-pulse-logical-import-v2\0");
    if let Some(source_account_id) = plan.selected_source_account_id {
        hasher.update(source_account_id.to_le_bytes());
    }
    let mut identities = Vec::new();
    collect_plan_identities(&mut identities, &plan.profiles);
    collect_plan_identities(&mut identities, &plan.source_machines);
    collect_plan_identities(&mut identities, &plan.snapshots);
    collect_plan_identities(&mut identities, &plan.tokens);
    collect_plan_identities(&mut identities, &plan.contexts);
    collect_plan_identities(&mut identities, &plan.gemini);
    collect_plan_identities(&mut identities, &plan.pricing_overrides);
    collect_plan_identities(&mut identities, &plan.alert_subscriptions);
    collect_plan_identities(&mut identities, &plan.alert_events);
    identities.sort_unstable();
    for (table, row_id, target_key, payload) in identities {
        for value in [table, row_id, target_key, payload] {
            hasher.update(value.as_bytes());
            hasher.update(b"\0");
        }
    }
    hex_digest(hasher.finalize())
}

fn validate_plan_identities(plan: &ImportPlan) -> PulseResult<()> {
    let mut identities = Vec::new();
    collect_plan_identities(&mut identities, &plan.profiles);
    collect_plan_identities(&mut identities, &plan.source_machines);
    collect_plan_identities(&mut identities, &plan.snapshots);
    collect_plan_identities(&mut identities, &plan.tokens);
    collect_plan_identities(&mut identities, &plan.contexts);
    collect_plan_identities(&mut identities, &plan.gemini);
    collect_plan_identities(&mut identities, &plan.pricing_overrides);
    collect_plan_identities(&mut identities, &plan.alert_subscriptions);
    collect_plan_identities(&mut identities, &plan.alert_events);
    let mut source_rows = BTreeSet::new();
    let mut targets = BTreeSet::new();
    for (table, row_id, target_key, _) in identities {
        if !source_rows.insert((table, row_id)) || !targets.insert((table, target_key)) {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "legacy Pulse SQLite contains duplicate logical import identities",
            ));
        }
    }
    Ok(())
}

fn collect_plan_identities<'a, T>(
    identities: &mut Vec<(&'a str, &'a str, &'a str, &'a str)>,
    rows: &'a [Planned<T>],
) {
    identities.extend(rows.iter().map(|row| {
        (
            row.source_table,
            row.source_row_id.as_str(),
            row.target_key.as_str(),
            row.payload_fingerprint.as_str(),
        )
    }));
}

fn apply_machine_alias(
    request: &ImportRequest,
    plan: &mut ImportPlan,
    machine: MachineName,
) -> MachineName {
    let Some(target) = request.machine_aliases.get(&machine) else {
        return machine;
    };
    let count = plan.machine_alias_counts.entry(machine).or_default();
    *count = count.saturating_add(1);
    target.clone()
}

fn finish_machine_aliases(request: &ImportRequest, plan: &mut ImportPlan) -> PulseResult<()> {
    for (source, target) in &request.machine_aliases {
        let count = plan
            .machine_alias_counts
            .get(source)
            .copied()
            .unwrap_or_default();
        if count == 0 {
            return Err(PulseError::invalid_input(
                "Pulse import machine alias did not match the selected legacy account",
            ));
        }
        plan.diagnostic(
            request.limits,
            ImportDiagnosticCode::MachineAliasApplied,
            None,
            Some(format!("{source}->{target}")),
            Some("machine"),
            format!("explicit machine alias applied to {count} selected rows"),
        );
    }
    Ok(())
}

fn normalize_plan_collisions(plan: &mut ImportPlan) -> PulseResult<()> {
    for (table, count) in [
        dedupe_identical_targets(&mut plan.profiles)?,
        dedupe_identical_targets(&mut plan.source_machines)?,
        dedupe_identical_targets(&mut plan.snapshots)?,
        dedupe_identical_targets(&mut plan.tokens)?,
        dedupe_identical_targets(&mut plan.contexts)?,
        dedupe_identical_targets(&mut plan.gemini)?,
        dedupe_identical_targets(&mut plan.pricing_overrides)?,
        dedupe_identical_targets(&mut plan.alert_subscriptions)?,
        dedupe_identical_targets(&mut plan.alert_events)?,
    ]
    .into_iter()
    .flatten()
    {
        let stats = plan.tables.entry(table.to_owned()).or_default();
        stats.planned = stats.planned.saturating_sub(count);
        stats.skipped = stats.skipped.saturating_add(count);
    }
    Ok(())
}

fn dedupe_identical_targets<T>(
    rows: &mut Vec<Planned<T>>,
) -> PulseResult<Option<(&'static str, usize)>> {
    let Some(table) = rows.first().map(|row| row.source_table) else {
        return Ok(None);
    };
    let mut seen = BTreeMap::<String, String>::new();
    let before = rows.len();
    let mut conflict = false;
    rows.retain(|row| match seen.get(&row.target_key) {
        None => {
            seen.insert(row.target_key.clone(), row.payload_fingerprint.clone());
            true
        }
        Some(payload) if payload == &row.payload_fingerprint => false,
        Some(_) => {
            conflict = true;
            true
        }
    });
    if conflict {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "legacy Pulse SQLite machine aliases create conflicting logical rows",
        ));
    }
    Ok(Some((table, before.saturating_sub(rows.len()))))
}

fn canonical_payload_fingerprint<T: Serialize>(value: &T) -> PulseResult<String> {
    let encoded = serde_json::to_vec(value).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Internal,
            "failed to fingerprint a typed Pulse import row",
        )
    })?;
    Ok(hex_digest(Sha256::digest(encoded)))
}

fn verify_opened_main(
    connection: &Connection,
    canonical: &Path,
    witnesses: &[PathWitness],
) -> PulseResult<()> {
    let opened = connection
        .query_row("PRAGMA database_list", [], |row| row.get::<_, String>(2))
        .map_err(|_| source_error())?;
    let opened = fs::canonicalize(opened).map_err(|_| source_error())?;
    if opened != canonical {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "SQLite opened a different legacy Pulse source than was validated",
        ));
    }
    verify_witnesses(witnesses)
}

fn source_tables(connection: &Connection, limits: ImportLimits) -> PulseResult<BTreeSet<String>> {
    let count = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_| source_error())?;
    let count = usize::try_from(count).map_err(|_| source_error())?;
    if count > limits.max_tables {
        return Err(PulseError::invalid_input(
            "legacy Pulse SQLite exceeds its table limit",
        ));
    }
    let max_name = connection
        .query_row(
            "SELECT COALESCE(MAX(length(CAST(name AS BLOB))),0) FROM sqlite_schema WHERE type='table'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_| source_error())?;
    if usize::try_from(max_name).map_err(|_| source_error())? > limits.max_text_bytes {
        return Err(PulseError::invalid_input(
            "legacy Pulse SQLite contains an oversized table name",
        ));
    }
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .map_err(|_| source_error())?;
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| source_error())?
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|_| source_error())
}

fn resolve_source_account(
    connection: &Connection,
    tables: &BTreeSet<String>,
    requested: Option<i64>,
) -> PulseResult<Option<i64>> {
    if requested.is_some() {
        return Ok(requested);
    }
    let mut ids = BTreeSet::new();
    for table in SUPPORTED_TABLES {
        if !tables.contains(*table) || !table_columns(connection, table)?.contains("account_id") {
            continue;
        }
        let sql = format!(
            "SELECT DISTINCT account_id FROM \"{table}\" WHERE account_id IS NOT NULL LIMIT 3"
        );
        let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
        let rows = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(|_| source_error())?;
        for id in rows {
            let id = id.map_err(|_| source_error())?;
            if id <= 0 {
                return Err(PulseError::invalid_input(
                    "legacy Pulse SQLite contains an invalid account id",
                ));
            }
            ids.insert(id);
            if ids.len() > 1 {
                return Err(PulseError::invalid_input(
                    "legacy Pulse SQLite contains multiple accounts; select one explicitly",
                ));
            }
        }
    }
    Ok(ids.into_iter().next())
}

fn preflight_tables(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    plan: &mut ImportPlan,
) -> PulseResult<()> {
    let mut total_rows = 0_usize;
    let mut total_text = 0_usize;
    for table in SUPPORTED_TABLES {
        if !tables.contains(*table) {
            plan.diagnostic(
                request.limits,
                ImportDiagnosticCode::MissingTable,
                Some(table),
                None,
                None,
                "optional legacy source table is absent",
            );
            continue;
        }
        let (discovered, output_count, table_text) =
            preflight_table(connection, table, source_account_id, request, plan)?;
        total_rows = total_rows
            .checked_add(output_count)
            .ok_or_else(source_error)?;
        if total_rows > request.limits.max_total_rows {
            return Err(PulseError::invalid_input(
                "legacy Pulse SQLite exceeds its total row limit",
            ));
        }
        plan.tables
            .entry((*table).to_owned())
            .or_default()
            .discovered = discovered;
        total_text = total_text
            .checked_add(table_text)
            .ok_or_else(source_error)?;
        if total_text > request.limits.max_total_text_bytes {
            return Err(PulseError::invalid_input(
                "legacy Pulse SQLite selected text exceeds its total byte limit",
            ));
        }
    }
    diagnose_excluded_tables(tables, request, plan);
    Ok(())
}

fn preflight_table(
    connection: &Connection,
    table: &str,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    plan: &mut ImportPlan,
) -> PulseResult<(usize, usize, usize)> {
    let columns = table_columns(connection, table)?;
    if !columns.contains("account_id") {
        plan.diagnostic(
            request.limits,
            ImportDiagnosticCode::SourceAccountColumnAbsent,
            Some(table),
            None,
            Some("account_id"),
            "legacy single-account table has no source account column",
        );
    } else if unscoped_account_rows(connection, table)? > 0 {
        plan.diagnostic(
            request.limits,
            ImportDiagnosticCode::SourceAccountRowUnscoped,
            Some(table),
            None,
            Some("account_id"),
            if source_account_id.is_some() {
                "rows without a source account id were excluded from the selected account"
            } else {
                "rows without a source account id are treated as legacy single-account data"
            },
        );
    }
    let discovered = scoped_count(connection, table, &columns, source_account_id)?;
    let output_count = if table == "gemini_quota" {
        gemini_output_count(connection, &columns, source_account_id)?
    } else {
        discovered
    };
    if output_count > request.limits.max_rows_per_table {
        return Err(PulseError::invalid_input(
            "legacy Pulse SQLite table exceeds its row limit",
        ));
    }
    let text_columns = selected_text_columns(table, &columns);
    let text = if table == "gemini_quota" {
        preflight_gemini_text(
            connection,
            &columns,
            &text_columns,
            source_account_id,
            request.limits,
        )?
    } else {
        preflight_text(
            connection,
            table,
            &columns,
            &text_columns,
            source_account_id,
            request.limits,
        )?
    };
    Ok((discovered, output_count, text))
}

fn diagnose_excluded_tables(
    tables: &BTreeSet<String>,
    request: &ImportRequest,
    plan: &mut ImportPlan,
) {
    for (table, code) in KNOWN_EXCLUDED_TABLES {
        if tables.contains(*table) {
            plan.diagnostic(
                request.limits,
                *code,
                Some(table),
                None,
                None,
                match code {
                    ImportDiagnosticCode::IngestTokensExcluded => {
                        "legacy ingest token hashes are excluded from import"
                    }
                    ImportDiagnosticCode::LossyLegacyRollupExcluded => {
                        "coarse legacy token rollups cannot be mapped without inventing token dimensions"
                    }
                    ImportDiagnosticCode::AuthoritativePricingDefaultsSeeded => {
                        "legacy pricing defaults are replaced by the target's authoritative seeded defaults"
                    }
                    _ => "legacy table has no lossless native import mapping",
                },
            );
        }
    }
}

fn selected_text_columns(table: &str, columns: &BTreeSet<String>) -> Vec<&'static str> {
    let candidates: &[&str] = match table {
        "profiles" => &["name", "config_dir", "vendor"],
        "machines" => &["name", "first_seen", "last_seen"],
        "usage_snapshots" => &[
            "profile",
            "five_hour_resets_at",
            "seven_day_resets_at",
            "polled_at",
            "machine",
            "reporter_version",
        ],
        "token_usage" => &[
            "profile",
            "machine",
            "session_id",
            "model",
            "settings_hash",
            "settings_json",
            "day",
            "source",
            "updated_at",
        ],
        "context_sessions" => &[
            "profile",
            "machine",
            "session_id",
            "model",
            "settings_json",
            "updated_at",
            "last_active_at",
        ],
        "gemini_quota" => &["timestamp", "model_id", "remaining_amount", "reset_time"],
        "pricing_overrides" => &["model", "settings_match_json", "updated_at"],
        "alert_subscriptions" => &["profile", "alert_type", "channel", "created_at"],
        "alert_events" => &["profile", "alert_type", "message", "triggered_at"],
        _ => &[],
    };
    candidates
        .iter()
        .copied()
        .filter(|column| columns.contains(*column))
        .collect()
}

fn preflight_text(
    connection: &Connection,
    table: &str,
    columns: &BTreeSet<String>,
    text_columns: &[&str],
    source_account_id: Option<i64>,
    limits: ImportLimits,
) -> PulseResult<usize> {
    if text_columns.is_empty() {
        return Ok(0);
    }
    let maximum = text_columns
        .iter()
        .map(|column| format!("COALESCE(MAX(length(CAST(\"{column}\" AS BLOB))),0)"))
        .collect::<Vec<_>>()
        .join(",");
    let total_expression = text_columns
        .iter()
        .map(|column| format!("COALESCE(length(CAST(\"{column}\" AS BLOB)),0)"))
        .collect::<Vec<_>>()
        .join("+");
    let (scope, parameters) = account_scope(columns, source_account_id);
    let sql =
        format!("SELECT {maximum}, COALESCE(SUM({total_expression}),0) FROM \"{table}\"{scope}");
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    let mut rows = statement
        .query(params_from_iter(parameters))
        .map_err(|_| source_error())?;
    let row = rows
        .next()
        .map_err(|_| source_error())?
        .ok_or_else(source_error)?;
    for index in 0..text_columns.len() {
        let length = row.get::<_, i64>(index).map_err(|_| source_error())?;
        if usize::try_from(length).map_err(|_| source_error())? > limits.max_text_bytes {
            return Err(PulseError::invalid_input(
                "legacy Pulse SQLite contains selected text above its field byte limit",
            ));
        }
    }
    let total = row
        .get::<_, i64>(text_columns.len())
        .map_err(|_| source_error())?;
    usize::try_from(total).map_err(|_| source_error())
}

fn preflight_gemini_text(
    connection: &Connection,
    columns: &BTreeSet<String>,
    text_columns: &[&str],
    source_account_id: Option<i64>,
    limits: ImportLimits,
) -> PulseResult<usize> {
    if text_columns.is_empty() {
        return Ok(0);
    }
    let maximum = text_columns
        .iter()
        .map(|column| format!("COALESCE(MAX(length(CAST(\"{column}\" AS BLOB))),0)"))
        .collect::<Vec<_>>()
        .join(",");
    let total_expression = text_columns
        .iter()
        .map(|column| format!("COALESCE(length(CAST(\"{column}\" AS BLOB)),0)"))
        .collect::<Vec<_>>()
        .join("+");
    let (scope, parameters) = account_scope(columns, source_account_id);
    let sql = format!(
        "WITH ranked AS (\
           SELECT *, ROW_NUMBER() OVER (\
             PARTITION BY model_id ORDER BY timestamp DESC, id DESC\
           ) AS atmux_import_rank \
           FROM gemini_quota{scope}\
         ) \
         SELECT {maximum}, COALESCE(SUM({total_expression}),0) \
         FROM ranked WHERE atmux_import_rank=1"
    );
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    let mut rows = statement
        .query(params_from_iter(parameters))
        .map_err(|_| source_error())?;
    let row = rows
        .next()
        .map_err(|_| source_error())?
        .ok_or_else(source_error)?;
    for index in 0..text_columns.len() {
        let length = row.get::<_, i64>(index).map_err(|_| source_error())?;
        if usize::try_from(length).map_err(|_| source_error())? > limits.max_text_bytes {
            return Err(PulseError::invalid_input(
                "legacy Pulse SQLite contains selected text above its field byte limit",
            ));
        }
    }
    let total = row
        .get::<_, i64>(text_columns.len())
        .map_err(|_| source_error())?;
    usize::try_from(total).map_err(|_| source_error())
}

fn scoped_count(
    connection: &Connection,
    table: &str,
    columns: &BTreeSet<String>,
    source_account_id: Option<i64>,
) -> PulseResult<usize> {
    let (scope, parameters) = account_scope(columns, source_account_id);
    let sql = format!("SELECT COUNT(*) FROM \"{table}\"{scope}");
    let count = connection
        .query_row(&sql, params_from_iter(parameters), |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|_| source_error())?;
    usize::try_from(count).map_err(|_| source_error())
}

fn gemini_output_count(
    connection: &Connection,
    columns: &BTreeSet<String>,
    source_account_id: Option<i64>,
) -> PulseResult<usize> {
    let (scope, parameters) = account_scope(columns, source_account_id);
    let sql = format!(
        "SELECT COUNT(*) FROM (\
           SELECT model_id FROM gemini_quota{scope} GROUP BY model_id\
         )"
    );
    let count = connection
        .query_row(&sql, params_from_iter(parameters), |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|_| source_error())?;
    usize::try_from(count).map_err(|_| source_error())
}

fn unscoped_account_rows(connection: &Connection, table: &str) -> PulseResult<usize> {
    let sql = format!("SELECT COUNT(*) FROM \"{table}\" WHERE account_id IS NULL");
    let count = connection
        .query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map_err(|_| source_error())?;
    usize::try_from(count).map_err(|_| source_error())
}

fn account_scope(
    columns: &BTreeSet<String>,
    source_account_id: Option<i64>,
) -> (&'static str, Vec<i64>) {
    if columns.contains("account_id")
        && let Some(id) = source_account_id
    {
        return (" WHERE account_id = ?1", vec![id]);
    }
    ("", Vec::new())
}

fn table_columns(connection: &Connection, table: &str) -> PulseResult<BTreeSet<String>> {
    let sql = format!("PRAGMA table_info(\"{table}\")");
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| source_error())?
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|_| source_error())
}

fn require_columns(
    connection: &Connection,
    table: &'static str,
    required: &[&str],
    request: &ImportRequest,
    plan: &mut ImportPlan,
) -> PulseResult<Option<BTreeSet<String>>> {
    let columns = table_columns(connection, table)?;
    let missing = required
        .iter()
        .filter(|column| !columns.contains(**column))
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(Some(columns));
    }
    for column in missing {
        plan.diagnostic(
            request.limits,
            ImportDiagnosticCode::MissingColumn,
            Some(table),
            None,
            Some(column),
            "legacy table column is required for a lossless native mapping",
        );
    }
    let stats = plan.tables.entry(table.to_owned()).or_default();
    stats.skipped = stats.discovered;
    Ok(None)
}

fn read_profiles(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    plan: &mut ImportPlan,
) -> PulseResult<BTreeMap<ProfileName, Vendor>> {
    let mut vendors = BTreeMap::new();
    if !tables.contains("profiles") {
        return Ok(vendors);
    }
    let Some(columns) = require_columns(
        connection,
        "profiles",
        &[
            "name",
            "config_dir",
            "poll_interval_minutes",
            "vendor",
            "monthly_budget_usd",
            "api_key",
        ],
        request,
        plan,
    )?
    else {
        return Ok(vendors);
    };
    let hidden = if columns.contains("hidden") {
        "hidden"
    } else {
        plan.diagnostic(
            request.limits,
            ImportDiagnosticCode::LegacyProfileVisibilityDefaulted,
            Some("profiles"),
            None,
            Some("hidden"),
            "legacy profile visibility is absent; imported profiles default to visible",
        );
        "0"
    };
    let (scope, parameters) = account_scope(&columns, source_account_id);
    let sql = format!(
        "SELECT name,config_dir,poll_interval_minutes,vendor,monthly_budget_usd,{hidden},\
         CASE WHEN api_key IS NOT NULL AND length(CAST(api_key AS BLOB)) > 0 THEN 1 ELSE 0 END \
         FROM profiles{scope} ORDER BY name"
    );
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    let mut rows = statement
        .query(params_from_iter(parameters))
        .map_err(|_| source_error())?;
    while let Some(row) = rows.next().map_err(|_| source_error())? {
        let raw_id = row.get::<_, String>(0).ok();
        let source_row_id = raw_id
            .clone()
            .unwrap_or_else(|| "unreadable-profile".to_owned());
        let parsed = parse_profile(row, request, raw_id);
        let profile = match parsed {
            Ok(profile) => profile,
            Err(ProfileParseError::CredentialRequired) => {
                plan.skip("profiles");
                plan.diagnostic(
                    request.limits,
                    ImportDiagnosticCode::CredentialReferenceRequired,
                    Some("profiles"),
                    Some(source_row_id),
                    Some("api_key"),
                    "profile requires an explicit external credential reference",
                );
                continue;
            }
            Err(ProfileParseError::Invalid) => {
                invalid_row(plan, request, "profiles", source_row_id);
                continue;
            }
        };
        let has_secret = row.get::<_, i64>(6).unwrap_or_default() != 0;
        if has_secret {
            let externalized = request.credentials.contains_key(&profile.name);
            plan.diagnostic(
                request.limits,
                if externalized {
                    ImportDiagnosticCode::InlineSecretExternalized
                } else {
                    ImportDiagnosticCode::InlineSecretExcluded
                },
                Some("profiles"),
                Some(profile.name.as_str().to_owned()),
                Some("api_key"),
                if externalized {
                    "legacy inline API key was replaced by an explicit external reference"
                } else {
                    "legacy inline API key was deliberately excluded"
                },
            );
        }
        vendors.insert(profile.name.clone(), profile.vendor);
        plan.profiles.push(Planned::new(
            "profiles",
            profile.name.as_str().to_owned(),
            format!("profile:{}", profile.name),
            profile,
        )?);
        plan.planned("profiles");
    }
    Ok(vendors)
}

enum ProfileParseError {
    CredentialRequired,
    Invalid,
}

fn parse_profile(
    row: &Row<'_>,
    request: &ImportRequest,
    raw_name: Option<String>,
) -> Result<Profile, ProfileParseError> {
    let name = ProfileName::new(raw_name.ok_or(ProfileParseError::Invalid)?)
        .map_err(|_| ProfileParseError::Invalid)?;
    let vendor = parse_vendor(
        &row.get::<_, String>(3)
            .map_err(|_| ProfileParseError::Invalid)?,
    )
    .ok_or(ProfileParseError::Invalid)?;
    let mut api_key_env = None;
    let mut api_key_file = None;
    if let Some(reference) = request.credentials.get(&name) {
        match reference {
            ExternalCredential::Environment(value) => api_key_env = Some(value.clone()),
            ExternalCredential::File(value) => api_key_file = Some(value.clone()),
        }
    }
    if vendor == Vendor::DeepseekBalance && api_key_env.is_none() && api_key_file.is_none() {
        return Err(ProfileParseError::CredentialRequired);
    }
    let interval = row
        .get::<_, i64>(2)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(ProfileParseError::Invalid)?;
    let hidden = match row
        .get::<_, i64>(5)
        .map_err(|_| ProfileParseError::Invalid)?
    {
        0 => false,
        1 => true,
        _ => return Err(ProfileParseError::Invalid),
    };
    let profile = Profile {
        account_id: request.target_account_id,
        name,
        vendor,
        config_dir: Some(PathBuf::from(
            row.get::<_, String>(1)
                .map_err(|_| ProfileParseError::Invalid)?,
        )),
        poll_interval_minutes: interval,
        monthly_budget_usd: row
            .get::<_, Option<f64>>(4)
            .map_err(|_| ProfileParseError::Invalid)?,
        api_key_env,
        api_key_file,
        refresh: request.refresh,
        hidden,
        origin: ProfileOrigin::Local,
    };
    profile.validate().map_err(|_| ProfileParseError::Invalid)?;
    Ok(profile)
}

fn read_machines(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    plan: &mut ImportPlan,
) -> PulseResult<()> {
    if !tables.contains("machines") {
        return Ok(());
    }
    let Some(columns) = require_columns(
        connection,
        "machines",
        &["name", "first_seen", "last_seen"],
        request,
        plan,
    )?
    else {
        return Ok(());
    };
    let (scope, parameters) = account_scope(&columns, source_account_id);
    let sql = format!("SELECT name,first_seen,last_seen FROM machines{scope} ORDER BY name");
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    let mut rows = statement
        .query(params_from_iter(parameters))
        .map_err(|_| source_error())?;
    let mut canonical = BTreeMap::<MachineName, (String, Machine)>::new();
    while let Some(row) = rows.next().map_err(|_| source_error())? {
        let source_row_id = row
            .get::<_, String>(0)
            .unwrap_or_else(|_| "unreadable-machine".to_owned());
        let parsed = (|| -> PulseResult<Machine> {
            let name = MachineName::new(source_row_id.clone())?;
            let first_seen = legacy_instant(&row.get::<_, String>(1).map_err(|_| source_error())?)?;
            let last_seen = legacy_instant(&row.get::<_, String>(2).map_err(|_| source_error())?)?;
            if last_seen < first_seen {
                return Err(PulseError::invalid_input("invalid legacy machine interval"));
            }
            Ok(Machine {
                account_id: request.target_account_id,
                name,
                first_seen,
                last_seen,
            })
        })();
        let Ok(mut machine) = parsed else {
            invalid_row(plan, request, "machines", source_row_id);
            continue;
        };
        machine.name = apply_machine_alias(request, plan, machine.name);
        merge_machine(&mut plan.prerequisite_machines, machine.clone());
        canonical
            .entry(machine.name.clone())
            .and_modify(|(first_source_row_id, existing)| {
                if source_row_id < *first_source_row_id {
                    first_source_row_id.clone_from(&source_row_id);
                }
                existing.first_seen = existing.first_seen.min(machine.first_seen);
                existing.last_seen = existing.last_seen.max(machine.last_seen);
            })
            .or_insert((source_row_id, machine));
    }
    for (name, (source_row_id, machine)) in canonical {
        plan.source_machines.push(Planned::new(
            "machines",
            source_row_id,
            format!("machine:{name}"),
            machine,
        )?);
        plan.planned("machines");
    }
    Ok(())
}

fn read_snapshots(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    profile_vendors: &BTreeMap<ProfileName, Vendor>,
    plan: &mut ImportPlan,
) -> PulseResult<()> {
    if !tables.contains("usage_snapshots") {
        return Ok(());
    }
    let Some(columns) = require_columns(
        connection,
        "usage_snapshots",
        &[
            "id",
            "profile",
            "five_hour_pct",
            "five_hour_resets_at",
            "seven_day_pct",
            "seven_day_resets_at",
            "polled_at",
        ],
        request,
        plan,
    )?
    else {
        return Ok(());
    };
    let (machine, reporter) = snapshot_optional_columns(&columns, request, plan);
    let (scope, parameters) = account_scope(&columns, source_account_id);
    let sql = format!(
        "SELECT id,profile,five_hour_pct,five_hour_resets_at,seven_day_pct,\
         seven_day_resets_at,polled_at,{machine},{reporter} FROM usage_snapshots{scope} \
         ORDER BY polled_at,id"
    );
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    let mut rows = statement
        .query(params_from_iter(parameters))
        .map_err(|_| source_error())?;
    while let Some(row) = rows.next().map_err(|_| source_error())? {
        let source_row_id = row
            .get::<_, i64>(0)
            .map_or_else(|_| "unreadable-snapshot".to_owned(), |id| id.to_string());
        match parse_snapshot(row, request, profile_vendors) {
            Ok(mut snapshot) => {
                snapshot.machine = apply_machine_alias(request, plan, snapshot.machine);
                observe_machine(
                    &mut plan.prerequisite_machines,
                    request.target_account_id,
                    snapshot.machine.clone(),
                    snapshot.polled_at,
                );
                plan.snapshots.push(Planned::new(
                    "usage_snapshots",
                    source_row_id,
                    snapshot_target_key(&snapshot),
                    snapshot,
                )?);
                plan.planned("usage_snapshots");
            }
            Err(RowParseError::MachineRequired) => {
                plan.skip("usage_snapshots");
                plan.diagnostic(
                    request.limits,
                    ImportDiagnosticCode::ExplicitMachineRequired,
                    Some("usage_snapshots"),
                    Some(source_row_id),
                    Some("machine"),
                    "snapshot row has no machine and no explicit fallback was provided",
                );
            }
            Err(RowParseError::Dependency) => {
                missing_dependency(plan, request, "usage_snapshots", source_row_id);
            }
            Err(RowParseError::Unrepresentable) => {
                unrepresentable_row(plan, request, "usage_snapshots", source_row_id);
            }
            Err(RowParseError::Invalid) => {
                invalid_row(plan, request, "usage_snapshots", source_row_id);
            }
        }
    }
    Ok(())
}

fn snapshot_optional_columns(
    columns: &BTreeSet<String>,
    request: &ImportRequest,
    plan: &mut ImportPlan,
) -> (&'static str, &'static str) {
    if columns.contains("raw_response") {
        plan.diagnostic(
            request.limits,
            ImportDiagnosticCode::RawProviderResponseExcluded,
            Some("usage_snapshots"),
            None,
            Some("raw_response"),
            "raw legacy provider responses are excluded from import",
        );
    }
    if !columns.contains("machine") {
        plan.diagnostic(
            request.limits,
            ImportDiagnosticCode::MissingColumn,
            Some("usage_snapshots"),
            None,
            Some("machine"),
            if request.fallback_machine.is_some() {
                "legacy machine column is absent; explicit fallback attribution will be used"
            } else {
                "legacy machine column is absent and no fallback attribution was provided"
            },
        );
    }
    if !columns.contains("machine") && request.fallback_machine.is_none() {
        plan.diagnostic(
            request.limits,
            ImportDiagnosticCode::ExplicitMachineRequired,
            Some("usage_snapshots"),
            None,
            Some("machine"),
            "legacy snapshots without machine attribution require an explicit fallback machine",
        );
    }
    if !columns.contains("reporter_version") {
        plan.diagnostic(
            request.limits,
            ImportDiagnosticCode::MissingColumn,
            Some("usage_snapshots"),
            None,
            Some("reporter_version"),
            "legacy reporter version is absent and remains explicitly unknown",
        );
    }
    let machine = if columns.contains("machine") {
        "machine"
    } else {
        "NULL"
    };
    let reporter = if columns.contains("reporter_version") {
        "reporter_version"
    } else {
        "NULL"
    };
    (machine, reporter)
}

#[derive(Clone, Copy)]
enum RowParseError {
    MachineRequired,
    Dependency,
    Invalid,
    Unrepresentable,
}

fn parse_snapshot(
    row: &Row<'_>,
    request: &ImportRequest,
    profile_vendors: &BTreeMap<ProfileName, Vendor>,
) -> Result<UsageSnapshot, RowParseError> {
    let profile = ProfileName::new(
        row.get::<_, String>(1)
            .map_err(|_| RowParseError::Invalid)?,
    )
    .map_err(|_| RowParseError::Invalid)?;
    let vendor = profile_vendors
        .get(&profile)
        .copied()
        .ok_or(RowParseError::Dependency)?;
    let machine = row
        .get::<_, Option<String>>(7)
        .map_err(|_| RowParseError::Invalid)?
        .filter(|value| !value.is_empty())
        .map(MachineName::new)
        .transpose()
        .map_err(|_| RowParseError::Invalid)?
        .or_else(|| request.fallback_machine.clone())
        .ok_or(RowParseError::MachineRequired)?;
    let polled_at = legacy_instant(
        &row.get::<_, String>(6)
            .map_err(|_| RowParseError::Invalid)?,
    )
    .map_err(|_| RowParseError::Invalid)?;
    let five = paired_window(
        row.get::<_, Option<f64>>(2)
            .map_err(|_| RowParseError::Invalid)?,
        row.get::<_, Option<String>>(3)
            .map_err(|_| RowParseError::Invalid)?,
        QuotaWindowKind::FiveHour,
    )?;
    let long_kind = match vendor {
        Vendor::AnthropicOauth => QuotaWindowKind::RollingSevenDay,
        Vendor::OpenaiCodex | Vendor::XaiGrok => QuotaWindowKind::FixedWeekly,
        Vendor::DeepseekBalance => QuotaWindowKind::MonthlyBudget,
        Vendor::Gemini | Vendor::Antigravity => return Err(RowParseError::Unrepresentable),
    };
    let long = paired_window(
        row.get::<_, Option<f64>>(4)
            .map_err(|_| RowParseError::Invalid)?,
        row.get::<_, Option<String>>(5)
            .map_err(|_| RowParseError::Invalid)?,
        long_kind,
    )?;
    let mut windows = Vec::new();
    if let Some(window) = five {
        if !vendor.allows_window(window.kind) {
            return Err(RowParseError::Unrepresentable);
        }
        windows.push(window);
    }
    if let Some(window) = long {
        windows.push(window);
    }
    if windows.is_empty() {
        return Err(RowParseError::Unrepresentable);
    }
    let snapshot = UsageSnapshot {
        account_id: request.target_account_id,
        profile,
        machine,
        vendor,
        windows,
        outcome: CollectionOutcome::Success,
        polled_at,
        reporter_version: row
            .get::<_, Option<String>>(8)
            .map_err(|_| RowParseError::Invalid)?,
    };
    snapshot.validate().map_err(|_| RowParseError::Invalid)?;
    Ok(snapshot)
}

fn paired_window(
    percent: Option<f64>,
    reset: Option<String>,
    kind: QuotaWindowKind,
) -> Result<Option<QuotaWindow>, RowParseError> {
    match (percent, reset) {
        (None, None) => Ok(None),
        (Some(percent), Some(reset)) => Ok(Some(QuotaWindow {
            kind,
            used_percent: Percent::new(percent).map_err(|_| RowParseError::Invalid)?,
            resets_at: legacy_instant(&reset).map_err(|_| RowParseError::Invalid)?,
        })),
        _ => Err(RowParseError::Unrepresentable),
    }
}

fn read_tokens(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    profile_vendors: &BTreeMap<ProfileName, Vendor>,
    plan: &mut ImportPlan,
) -> PulseResult<()> {
    if !tables.contains("token_usage") {
        return Ok(());
    }
    let Some(columns) = require_columns(
        connection,
        "token_usage",
        &[
            "id",
            "profile",
            "machine",
            "session_id",
            "model",
            "settings_json",
            "day",
            "tokens_in",
            "tokens_out",
            "cache_write_5m",
            "cache_write_1h",
            "cache_read",
            "source",
            "updated_at",
        ],
        request,
        plan,
    )?
    else {
        return Ok(());
    };
    let (scope, parameters) = account_scope(&columns, source_account_id);
    let sql = format!(
        "SELECT id,profile,machine,session_id,model,settings_json,day,tokens_in,tokens_out,\
         cache_write_5m,cache_write_1h,cache_read,source,updated_at FROM token_usage{scope} ORDER BY id"
    );
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    let mut rows = statement
        .query(params_from_iter(parameters))
        .map_err(|_| source_error())?;
    while let Some(row) = rows.next().map_err(|_| source_error())? {
        let source_row_id = row
            .get::<_, i64>(0)
            .map_or_else(|_| "unreadable-token".to_owned(), |id| id.to_string());
        match parse_token(row, request, profile_vendors) {
            Ok((mut grain, observed_at)) => {
                grain.machine = apply_machine_alias(request, plan, grain.machine);
                observe_machine(
                    &mut plan.prerequisite_machines,
                    request.target_account_id,
                    grain.machine.clone(),
                    observed_at,
                );
                plan.tokens.push(Planned::new(
                    "token_usage",
                    source_row_id,
                    token_target_key(&grain),
                    grain,
                )?);
                plan.planned("token_usage");
            }
            Err(RowParseError::Dependency) => {
                missing_dependency(plan, request, "token_usage", source_row_id);
            }
            Err(RowParseError::Unrepresentable) => {
                unrepresentable_row(plan, request, "token_usage", source_row_id);
            }
            _ => invalid_row(plan, request, "token_usage", source_row_id),
        }
    }
    Ok(())
}

fn parse_token(
    row: &Row<'_>,
    request: &ImportRequest,
    profile_vendors: &BTreeMap<ProfileName, Vendor>,
) -> Result<(TokenGrain, Instant), RowParseError> {
    let profile = ProfileName::new(
        row.get::<_, String>(1)
            .map_err(|_| RowParseError::Invalid)?,
    )
    .map_err(|_| RowParseError::Invalid)?;
    if !profile_vendors.contains_key(&profile) {
        return Err(RowParseError::Dependency);
    }
    let settings = parse_settings(
        &row.get::<_, String>(5)
            .map_err(|_| RowParseError::Invalid)?,
    )?;
    let source = match row
        .get::<_, String>(12)
        .map_err(|_| RowParseError::Invalid)?
        .as_str()
    {
        "local" => TokenSource::Local,
        "ingest" => TokenSource::Ingest,
        _ => return Err(RowParseError::Unrepresentable),
    };
    let settings_hash = settings.sha256().map_err(|_| RowParseError::Invalid)?;
    let grain = TokenGrain {
        account_id: request.target_account_id,
        profile,
        machine: MachineName::new(
            row.get::<_, String>(2)
                .map_err(|_| RowParseError::Invalid)?,
        )
        .map_err(|_| RowParseError::Invalid)?,
        session_id: SessionId::new(
            row.get::<_, String>(3)
                .map_err(|_| RowParseError::Invalid)?,
        )
        .map_err(|_| RowParseError::Invalid)?,
        model: row
            .get::<_, String>(4)
            .map_err(|_| RowParseError::Invalid)?,
        settings,
        settings_hash,
        day: row
            .get::<_, String>(6)
            .map_err(|_| RowParseError::Invalid)?,
        tokens_in: legacy_u64(row, 7)?,
        tokens_out: legacy_u64(row, 8)?,
        cache_write_5m: legacy_u64(row, 9)?,
        cache_write_1h: legacy_u64(row, 10)?,
        cache_read: legacy_u64(row, 11)?,
        source,
    };
    grain.validate().map_err(|_| RowParseError::Invalid)?;
    let observed_at = legacy_instant(
        &row.get::<_, String>(13)
            .map_err(|_| RowParseError::Invalid)?,
    )
    .map_err(|_| RowParseError::Invalid)?;
    Ok((grain, observed_at))
}

fn read_contexts(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    profile_vendors: &BTreeMap<ProfileName, Vendor>,
    plan: &mut ImportPlan,
) -> PulseResult<()> {
    if !tables.contains("context_sessions") {
        return Ok(());
    }
    let Some(columns) = require_columns(
        connection,
        "context_sessions",
        &[
            "profile",
            "machine",
            "session_id",
            "model",
            "settings_json",
            "context_tokens",
            "context_pct",
            "effective_limit",
            "updated_at",
            "last_active_at",
        ],
        request,
        plan,
    )?
    else {
        return Ok(());
    };
    let (scope, parameters) = account_scope(&columns, source_account_id);
    let sql = format!(
        "SELECT profile,machine,session_id,model,settings_json,context_tokens,context_pct,\
         effective_limit,updated_at,last_active_at FROM context_sessions{scope} \
         ORDER BY profile,machine,session_id"
    );
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    let mut rows = statement
        .query(params_from_iter(parameters))
        .map_err(|_| source_error())?;
    while let Some(row) = rows.next().map_err(|_| source_error())? {
        let source_row_id = context_source_id(row);
        match parse_context(row, request, profile_vendors) {
            Ok(mut context) => {
                context.machine = apply_machine_alias(request, plan, context.machine);
                observe_machine(
                    &mut plan.prerequisite_machines,
                    request.target_account_id,
                    context.machine.clone(),
                    context.last_active_at,
                );
                plan.contexts.push(Planned::new(
                    "context_sessions",
                    source_row_id,
                    context_target_key(&context),
                    context,
                )?);
                plan.planned("context_sessions");
            }
            Err(RowParseError::Dependency) => {
                missing_dependency(plan, request, "context_sessions", source_row_id);
            }
            Err(RowParseError::Unrepresentable) => {
                unrepresentable_row(plan, request, "context_sessions", source_row_id);
            }
            _ => invalid_row(plan, request, "context_sessions", source_row_id),
        }
    }
    Ok(())
}

fn parse_context(
    row: &Row<'_>,
    request: &ImportRequest,
    profile_vendors: &BTreeMap<ProfileName, Vendor>,
) -> Result<ContextSession, RowParseError> {
    let profile = ProfileName::new(
        row.get::<_, String>(0)
            .map_err(|_| RowParseError::Invalid)?,
    )
    .map_err(|_| RowParseError::Invalid)?;
    if !profile_vendors.contains_key(&profile) {
        return Err(RowParseError::Dependency);
    }
    let tokens = legacy_optional_u64(row, 5)?;
    let percent = row
        .get::<_, Option<f64>>(6)
        .map_err(|_| RowParseError::Invalid)?
        .map(Percent::new)
        .transpose()
        .map_err(|_| RowParseError::Invalid)?;
    let limit = legacy_optional_u64(row, 7)?;
    if percent.is_some() && (tokens.is_none() || limit.is_none()) {
        return Err(RowParseError::Unrepresentable);
    }
    let session = ContextSession {
        account_id: request.target_account_id,
        profile,
        machine: MachineName::new(
            row.get::<_, String>(1)
                .map_err(|_| RowParseError::Invalid)?,
        )
        .map_err(|_| RowParseError::Invalid)?,
        session_id: SessionId::new(
            row.get::<_, String>(2)
                .map_err(|_| RowParseError::Invalid)?,
        )
        .map_err(|_| RowParseError::Invalid)?,
        model: row
            .get::<_, Option<String>>(3)
            .map_err(|_| RowParseError::Invalid)?,
        settings: parse_settings(
            &row.get::<_, String>(4)
                .map_err(|_| RowParseError::Invalid)?,
        )?,
        context_tokens: tokens,
        context_percent: percent,
        effective_limit: limit,
        last_active_at: legacy_instant(
            &row.get::<_, String>(9)
                .map_err(|_| RowParseError::Invalid)?,
        )
        .map_err(|_| RowParseError::Invalid)?,
        last_reset_at: None,
        collected_at: legacy_instant(
            &row.get::<_, String>(8)
                .map_err(|_| RowParseError::Invalid)?,
        )
        .map_err(|_| RowParseError::Invalid)?,
    };
    session.validate().map_err(|_| RowParseError::Invalid)?;
    Ok(session)
}

fn read_gemini(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    plan: &mut ImportPlan,
) -> PulseResult<()> {
    if !tables.contains("gemini_quota") {
        return Ok(());
    }
    let Some(columns) = require_columns(
        connection,
        "gemini_quota",
        &[
            "id",
            "timestamp",
            "model_id",
            "remaining_fraction",
            "remaining_amount",
            "reset_time",
        ],
        request,
        plan,
    )?
    else {
        return Ok(());
    };
    let (scope, parameters) = account_scope(&columns, source_account_id);
    let sql = format!(
        "WITH ranked AS (\
           SELECT id,timestamp,model_id,remaining_fraction,remaining_amount,reset_time, \
             ROW_NUMBER() OVER (\
               PARTITION BY model_id ORDER BY timestamp DESC, id DESC\
             ) AS atmux_import_rank \
           FROM gemini_quota{scope}\
         ) \
         SELECT id,timestamp,model_id,remaining_fraction,remaining_amount,reset_time \
         FROM ranked WHERE atmux_import_rank=1 ORDER BY model_id"
    );
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    let mut rows = statement
        .query(params_from_iter(parameters))
        .map_err(|_| source_error())?;
    while let Some(row) = rows.next().map_err(|_| source_error())? {
        let source_row_id = row
            .get::<_, i64>(0)
            .map_or_else(|_| "unreadable-gemini".to_owned(), |id| id.to_string());
        let parsed = (|| -> Result<GeminiQuota, RowParseError> {
            let quota = GeminiQuota {
                account_id: request.target_account_id,
                collected_at: legacy_instant(
                    &row.get::<_, String>(1)
                        .map_err(|_| RowParseError::Invalid)?,
                )
                .map_err(|_| RowParseError::Invalid)?,
                model_id: row
                    .get::<_, String>(2)
                    .map_err(|_| RowParseError::Invalid)?,
                remaining_fraction: Fraction::new(
                    row.get::<_, f64>(3).map_err(|_| RowParseError::Invalid)?,
                )
                .map_err(|_| RowParseError::Invalid)?,
                remaining_amount: row
                    .get::<_, Option<String>>(4)
                    .map_err(|_| RowParseError::Invalid)?,
                resets_at: row
                    .get::<_, Option<String>>(5)
                    .map_err(|_| RowParseError::Invalid)?
                    .map(|value| legacy_instant(&value))
                    .transpose()
                    .map_err(|_| RowParseError::Invalid)?,
            };
            quota.validate().map_err(|_| RowParseError::Invalid)?;
            Ok(quota)
        })();
        let Ok(quota) = parsed else {
            invalid_row(plan, request, "gemini_quota", source_row_id);
            continue;
        };
        plan.gemini.push(Planned::new(
            "gemini_quota",
            source_row_id,
            format!("gemini:{}", quota.model_id),
            quota,
        )?);
        plan.planned("gemini_quota");
    }
    Ok(())
}

fn read_pricing_overrides(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    plan: &mut ImportPlan,
) -> PulseResult<()> {
    if !tables.contains("pricing_overrides") {
        return Ok(());
    }
    let Some(columns) = require_columns(
        connection,
        "pricing_overrides",
        &[
            "model",
            "settings_match_json",
            "input",
            "output",
            "cache_write_5m",
            "cache_write_1h",
            "cache_read",
        ],
        request,
        plan,
    )?
    else {
        return Ok(());
    };
    let (scope, parameters) = account_scope(&columns, source_account_id);
    let sql = format!(
        "SELECT model,settings_match_json,input,output,cache_write_5m,cache_write_1h,cache_read \
         FROM pricing_overrides{scope} ORDER BY model,settings_match_json"
    );
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    let mut rows = statement
        .query(params_from_iter(parameters))
        .map_err(|_| source_error())?;
    while let Some(row) = rows.next().map_err(|_| source_error())? {
        let model = row.get::<_, String>(0).map_err(|_| source_error())?;
        let settings_json = row.get::<_, String>(1).map_err(|_| source_error())?;
        let source_row_id = pricing_source_id(&model, &settings_json);
        let parsed = (|| -> PulseResult<PricingRule> {
            let settings_match = serde_json::from_str::<BTreeMap<String, String>>(&settings_json)
                .map_err(|_| {
                PulseError::invalid_input("legacy pricing settings are invalid")
            })?;
            let vendor = pricing_vendor(&model).ok_or_else(|| {
                PulseError::invalid_input("legacy pricing model has no authoritative vendor")
            })?;
            let key_digest = canonical_payload_fingerprint(&(model.as_str(), &settings_match))?;
            let rule = PricingRule {
                key: format!("legacy-{}", &key_digest[..32]),
                vendor,
                model_pattern: model,
                settings_match,
                input_per_million_usd: row.get(2).map_err(|_| source_error())?,
                output_per_million_usd: row.get(3).map_err(|_| source_error())?,
                cache_write_5m_per_million_usd: row.get(4).map_err(|_| source_error())?,
                cache_write_1h_per_million_usd: row.get(5).map_err(|_| source_error())?,
                cache_read_per_million_usd: row.get(6).map_err(|_| source_error())?,
            };
            rule.validate()?;
            Ok(rule)
        })();
        match parsed {
            Ok(rule) => {
                let target_key = format!("pricing-override:{}", rule.key);
                plan.pricing_overrides.push(Planned::new(
                    "pricing_overrides",
                    source_row_id,
                    target_key,
                    rule,
                )?);
                plan.planned("pricing_overrides");
            }
            Err(_) => unrepresentable_row(plan, request, "pricing_overrides", source_row_id),
        }
    }
    Ok(())
}

fn pricing_source_id(model: &str, settings_json: &str) -> String {
    let digest = Sha256::digest([model.as_bytes(), b"\0", settings_json.as_bytes()].concat());
    format!("pricing:{}", hex_digest(digest))
}

fn pricing_vendor(model: &str) -> Option<Vendor> {
    crate::pulse::pricing::authoritative_pricing()
        .into_iter()
        .find(|item| item.rule.model_pattern == model)
        .map(|item| item.rule.vendor)
        .or_else(|| {
            if model.starts_with("claude-") {
                Some(Vendor::AnthropicOauth)
            } else if model.starts_with("gpt-") {
                Some(Vendor::OpenaiCodex)
            } else if model.starts_with("deepseek") {
                Some(Vendor::DeepseekBalance)
            } else if model.starts_with("gemini-") {
                Some(Vendor::Gemini)
            } else if model.starts_with("antigravity-") {
                Some(Vendor::Antigravity)
            } else {
                None
            }
        })
}

fn parse_alert_subscription_row(
    row: &Row<'_>,
    legacy_id: i64,
    request: &ImportRequest,
    profile_vendors: &BTreeMap<ProfileName, Vendor>,
) -> PulseResult<(ImportedAlertSubscription, bool)> {
    if legacy_id <= 0 {
        return Err(PulseError::invalid_input("legacy alert id is invalid"));
    }
    let profile = ProfileName::new(row.get::<_, String>(1).map_err(|_| source_error())?)?;
    if !profile_vendors.contains_key(&profile) {
        return Err(PulseError::new(
            PulseErrorKind::NotFound,
            "legacy alert profile dependency is missing",
        ));
    }
    let alert_type = parse_alert_type(&row.get::<_, String>(2).map_err(|_| source_error())?)?;
    let threshold = row
        .get::<_, Option<f64>>(3)
        .map_err(|_| source_error())?
        .map(Percent::new)
        .transpose()?;
    let channel_excluded = row
        .get::<_, Option<String>>(4)
        .map_err(|_| source_error())?
        .is_some();
    let cooldown_minutes = u32::try_from(row.get::<_, i64>(5).map_err(|_| source_error())?)
        .map_err(|_| PulseError::invalid_input("legacy alert cooldown is invalid"))?;
    let enabled = match row.get::<_, i64>(6).map_err(|_| source_error())? {
        0 => false,
        1 => true,
        _ => return Err(PulseError::invalid_input("legacy alert enabled is invalid")),
    };
    let subscription = AlertSubscription {
        account_id: request.target_account_id,
        profile,
        alert_type,
        threshold,
        cooldown_minutes,
        delivery: None,
        enabled,
    };
    subscription.validate()?;
    Ok((
        ImportedAlertSubscription {
            subscription,
            created_at: legacy_instant(&row.get::<_, String>(7).map_err(|_| source_error())?)?,
        },
        channel_excluded,
    ))
}

fn read_alert_subscriptions(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    profile_vendors: &BTreeMap<ProfileName, Vendor>,
    plan: &mut ImportPlan,
) -> PulseResult<BTreeMap<i64, AlertSubscription>> {
    let mut by_legacy_id = BTreeMap::new();
    if !tables.contains("alert_subscriptions") {
        return Ok(by_legacy_id);
    }
    let Some(columns) = require_columns(
        connection,
        "alert_subscriptions",
        &[
            "id",
            "profile",
            "alert_type",
            "threshold",
            "channel",
            "cooldown_minutes",
            "enabled",
            "created_at",
        ],
        request,
        plan,
    )?
    else {
        return Ok(by_legacy_id);
    };
    let (scope, parameters) = account_scope(&columns, source_account_id);
    let sql = format!(
        "SELECT id,profile,alert_type,threshold,channel,cooldown_minutes,enabled,created_at \
         FROM alert_subscriptions{scope} ORDER BY id"
    );
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    let mut rows = statement
        .query(params_from_iter(parameters))
        .map_err(|_| source_error())?;
    while let Some(row) = rows.next().map_err(|_| source_error())? {
        let legacy_id = row.get::<_, i64>(0).map_err(|_| source_error())?;
        let source_row_id = legacy_id.to_string();
        let parsed = parse_alert_subscription_row(row, legacy_id, request, profile_vendors);
        match parsed {
            Ok((imported, channel_excluded)) => {
                if channel_excluded {
                    plan.diagnostic(
                        request.limits,
                        ImportDiagnosticCode::AlertDeliveryExcluded,
                        Some("alert_subscriptions"),
                        Some(source_row_id.clone()),
                        Some("channel"),
                        "legacy alert channel routing is not losslessly representable; pull visibility is preserved without delivery",
                    );
                }
                let target_key = alert_subscription_target_key(&imported.subscription)?;
                by_legacy_id.insert(legacy_id, imported.subscription.clone());
                plan.alert_subscriptions.push(Planned::new(
                    "alert_subscriptions",
                    source_row_id,
                    target_key,
                    imported,
                )?);
                plan.planned("alert_subscriptions");
            }
            Err(error) if error.kind() == PulseErrorKind::NotFound => {
                missing_dependency(plan, request, "alert_subscriptions", source_row_id);
            }
            Err(_) => invalid_row(plan, request, "alert_subscriptions", source_row_id),
        }
    }
    Ok(by_legacy_id)
}

fn parse_alert_event_row(
    row: &Row<'_>,
    legacy_id: i64,
    request: &ImportRequest,
    subscriptions: &BTreeMap<i64, AlertSubscription>,
) -> PulseResult<ImportedAlertEvent> {
    if legacy_id <= 0 {
        return Err(PulseError::invalid_input(
            "legacy alert event id is invalid",
        ));
    }
    let subscription_id = row.get::<_, i64>(1).map_err(|_| source_error())?;
    let subscription = subscriptions
        .get(&subscription_id)
        .cloned()
        .ok_or_else(|| {
            PulseError::new(
                PulseErrorKind::NotFound,
                "legacy alert subscription dependency is missing",
            )
        })?;
    let profile = ProfileName::new(row.get::<_, String>(2).map_err(|_| source_error())?)?;
    let alert_type = parse_alert_type(&row.get::<_, String>(3).map_err(|_| source_error())?)?;
    if profile != subscription.profile || alert_type != subscription.alert_type {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "legacy alert event disagrees with its subscription",
        ));
    }
    let message = row.get::<_, String>(4).map_err(|_| source_error())?;
    if message.is_empty() || message.len() > 4_096 {
        return Err(PulseError::invalid_input("legacy alert message is invalid"));
    }
    let current_value = row
        .get::<_, Option<f64>>(5)
        .map_err(|_| source_error())?
        .map(Percent::new)
        .transpose()?;
    let threshold = row
        .get::<_, Option<f64>>(6)
        .map_err(|_| source_error())?
        .map(Percent::new)
        .transpose()?;
    let acknowledged = match row.get::<_, i64>(7).map_err(|_| source_error())? {
        0 => false,
        1 => true,
        _ => {
            return Err(PulseError::invalid_input(
                "legacy alert acknowledgement is invalid",
            ));
        }
    };
    Ok(ImportedAlertEvent {
        subscription,
        input: AlertEventInput {
            account_id: request.target_account_id,
            subscription_id: 0,
            profile,
            alert_type,
            message,
            current_value,
            threshold,
            triggered_at: legacy_instant(&row.get::<_, String>(8).map_err(|_| source_error())?)?,
        },
        acknowledged,
    })
}

fn read_alert_events(
    connection: &Connection,
    tables: &BTreeSet<String>,
    source_account_id: Option<i64>,
    request: &ImportRequest,
    subscriptions: &BTreeMap<i64, AlertSubscription>,
    plan: &mut ImportPlan,
) -> PulseResult<()> {
    if !tables.contains("alert_events") {
        return Ok(());
    }
    let Some(columns) = require_columns(
        connection,
        "alert_events",
        &[
            "id",
            "subscription_id",
            "profile",
            "alert_type",
            "message",
            "current_value",
            "threshold",
            "acknowledged",
            "triggered_at",
        ],
        request,
        plan,
    )?
    else {
        return Ok(());
    };
    let (scope, parameters) = account_scope(&columns, source_account_id);
    let sql = format!(
        "SELECT id,subscription_id,profile,alert_type,message,current_value,threshold, \
         acknowledged,triggered_at FROM alert_events{scope} ORDER BY id"
    );
    let mut statement = connection.prepare(&sql).map_err(|_| source_error())?;
    let mut rows = statement
        .query(params_from_iter(parameters))
        .map_err(|_| source_error())?;
    while let Some(row) = rows.next().map_err(|_| source_error())? {
        let legacy_id = row.get::<_, i64>(0).map_err(|_| source_error())?;
        let source_row_id = legacy_id.to_string();
        let parsed = parse_alert_event_row(row, legacy_id, request, subscriptions);
        match parsed {
            Ok(event) => {
                plan.alert_events.push(Planned::new(
                    "alert_events",
                    source_row_id,
                    format!("legacy-alert-event:{legacy_id}"),
                    event,
                )?);
                plan.planned("alert_events");
            }
            Err(error) if error.kind() == PulseErrorKind::NotFound => {
                missing_dependency(plan, request, "alert_events", source_row_id);
            }
            Err(_) => invalid_row(plan, request, "alert_events", source_row_id),
        }
    }
    Ok(())
}

fn parse_alert_type(value: &str) -> PulseResult<AlertType> {
    match value {
        "five_hour_threshold" => Ok(AlertType::FiveHourThreshold),
        "seven_day_threshold" => Ok(AlertType::SevenDayThreshold),
        "auth_failure" => Ok(AlertType::AuthenticationFailure),
        "context_threshold" => Ok(AlertType::ContextThreshold),
        _ => Err(PulseError::invalid_input(
            "legacy alert type is unsupported",
        )),
    }
}

fn alert_subscription_target_key(subscription: &AlertSubscription) -> PulseResult<String> {
    let payload = canonical_payload_fingerprint(&(
        subscription.profile.as_str(),
        subscription.alert_type,
        subscription.threshold.map(Percent::get),
    ))?;
    Ok(format!("alert-subscription:{payload}"))
}

fn parse_settings(value: &str) -> Result<AgentSettings, RowParseError> {
    let serde_json::Value::Object(mut object) =
        serde_json::from_str::<serde_json::Value>(value).map_err(|_| RowParseError::Invalid)?
    else {
        return Err(RowParseError::Invalid);
    };
    let service_tier = take_optional_string(&mut object, "service_tier")?;
    let effort = take_optional_string(&mut object, "effort")?;
    let mut additional = BTreeMap::new();
    for (key, value) in object {
        match value {
            serde_json::Value::String(value) => {
                additional.insert(key, value);
            }
            serde_json::Value::Null => {}
            _ => return Err(RowParseError::Unrepresentable),
        }
    }
    Ok(AgentSettings {
        service_tier,
        effort,
        additional,
    })
}

fn take_optional_string(
    object: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<String>, RowParseError> {
    match object.remove(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(RowParseError::Unrepresentable),
    }
}

fn legacy_u64(row: &Row<'_>, index: usize) -> Result<u64, RowParseError> {
    row.get::<_, i64>(index)
        .map_err(|_| RowParseError::Invalid)
        .and_then(|value| u64::try_from(value).map_err(|_| RowParseError::Invalid))
}

fn legacy_optional_u64(row: &Row<'_>, index: usize) -> Result<Option<u64>, RowParseError> {
    row.get::<_, Option<i64>>(index)
        .map_err(|_| RowParseError::Invalid)?
        .map(u64::try_from)
        .transpose()
        .map_err(|_| RowParseError::Invalid)
}

fn legacy_instant(value: &str) -> PulseResult<Instant> {
    if let Ok(instant) = Instant::from_iso8601(value) {
        return Ok(instant);
    }
    if value.len() >= 19 && value.as_bytes().get(10) == Some(&b' ') {
        let mut normalized = value.to_owned();
        normalized.replace_range(10..11, "T");
        normalized.push('Z');
        return Instant::from_iso8601(&normalized);
    }
    Err(PulseError::invalid_input(
        "legacy timestamp has no lossless UTC interpretation",
    ))
}

fn parse_vendor(value: &str) -> Option<Vendor> {
    match value {
        "anthropic-oauth" => Some(Vendor::AnthropicOauth),
        "openai-codex" => Some(Vendor::OpenaiCodex),
        "deepseek-balance" => Some(Vendor::DeepseekBalance),
        "xai-grok" => Some(Vendor::XaiGrok),
        "antigravity" => Some(Vendor::Antigravity),
        _ => None,
    }
}

fn observe_machine(
    machines: &mut BTreeMap<MachineName, Machine>,
    account_id: AccountId,
    name: MachineName,
    observed_at: Instant,
) {
    merge_machine(
        machines,
        Machine {
            account_id,
            name,
            first_seen: observed_at,
            last_seen: observed_at,
        },
    );
}

fn merge_machine(machines: &mut BTreeMap<MachineName, Machine>, machine: Machine) {
    machines
        .entry(machine.name.clone())
        .and_modify(|existing| {
            existing.first_seen = existing.first_seen.min(machine.first_seen);
            existing.last_seen = existing.last_seen.max(machine.last_seen);
        })
        .or_insert(machine);
}

fn snapshot_target_key(snapshot: &UsageSnapshot) -> String {
    let mut hasher = Sha256::new();
    hasher.update(snapshot.profile.as_str());
    hasher.update(b"\0");
    hasher.update(snapshot.machine.as_str());
    hasher.update(b"\0");
    hasher.update(snapshot.polled_at.epoch_millis().to_le_bytes());
    format!("usage:{}", hex_digest(hasher.finalize()))
}

fn token_target_key(grain: &TokenGrain) -> String {
    let mut hasher = Sha256::new();
    for value in [
        grain.profile.as_str(),
        grain.machine.as_str(),
        grain.session_id.as_str(),
        grain.model.as_str(),
        grain.settings_hash.as_str(),
        grain.day.as_str(),
    ] {
        hasher.update(value.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(match grain.source {
        TokenSource::Local => b"local".as_slice(),
        TokenSource::Ingest => b"ingest".as_slice(),
    });
    format!("token:{}", hex_digest(hasher.finalize()))
}

fn context_source_id(row: &Row<'_>) -> String {
    let mut hasher = Sha256::new();
    for index in 0..3 {
        if let Ok(value) = row.get::<_, String>(index) {
            hasher.update(value.as_bytes());
        }
        hasher.update(b"\0");
    }
    hex_digest(hasher.finalize())
}

fn context_target_key(context: &ContextSession) -> String {
    let mut hasher = Sha256::new();
    for value in [
        context.profile.as_str(),
        context.machine.as_str(),
        context.session_id.as_str(),
    ] {
        hasher.update(value.as_bytes());
        hasher.update(b"\0");
    }
    format!("context:{}", hex_digest(hasher.finalize()))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn invalid_row(plan: &mut ImportPlan, request: &ImportRequest, table: &str, row_id: String) {
    plan.skip(table);
    plan.diagnostic(
        request.limits,
        ImportDiagnosticCode::InvalidSourceRow,
        Some(table),
        Some(row_id),
        None,
        "legacy row failed typed validation and was skipped",
    );
}

fn unrepresentable_row(
    plan: &mut ImportPlan,
    request: &ImportRequest,
    table: &str,
    row_id: String,
) {
    plan.skip(table);
    plan.diagnostic(
        request.limits,
        ImportDiagnosticCode::UnrepresentableSourceRow,
        Some(table),
        Some(row_id),
        None,
        "legacy row cannot be mapped without inventing absent semantics",
    );
}

fn missing_dependency(plan: &mut ImportPlan, request: &ImportRequest, table: &str, row_id: String) {
    plan.skip(table);
    plan.diagnostic(
        request.limits,
        ImportDiagnosticCode::MissingDependency,
        Some(table),
        Some(row_id),
        Some("profile"),
        "legacy row references a profile that was not safely importable",
    );
}

fn pragma_i64(connection: &Connection, pragma: &str) -> PulseResult<i64> {
    connection
        .query_row(pragma, [], |row| row.get(0))
        .map_err(|_| source_error())
}

fn source_error() -> PulseError {
    PulseError::new(
        PulseErrorKind::Storage,
        "failed to inspect legacy Pulse SQLite through the read-only boundary",
    )
}
