//! Bounded pull federation for Pulse data.
//!
//! Pulls use the existing authenticated [`RemoteMachine`] transport, so mTLS
//! and node-token policy stay identical to tmux federation. A page carries
//! explicit origin provenance; a direct peer may export only records whose
//! path contains that peer alone. This prevents coordinators from mirroring a
//! third node's Pulse data back into the federation.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Semaphore, watch},
    task::JoinSet,
};

#[cfg(test)]
use super::RefreshPolicy;
use super::{
    AccountId, ContextSession, Machine, MachineName, Profile, ProfileOrigin, TokenGrain,
    UsageSnapshot,
    error::{PulseError, PulseErrorKind, PulseResult},
    store::Store,
};
use crate::remote::RemoteMachine;

pub const FEDERATION_VERSION: u16 = 2;
pub const DEFAULT_PAGE_ROWS: u16 = 250;
pub const MAX_PAGE_ROWS: u16 = 500;
pub const MAX_CURSOR_BYTES: usize = 2_048;
pub const MAX_ORIGIN_HOPS: usize = 8;
pub const MAX_PAGES_PER_SYNC: usize = 32;
pub const MAX_CONFIGURED_PEERS: usize = 64;
pub const MAX_CONCURRENT_PEERS: usize = 8;

pub type FederationFuture<T> = Pin<Box<dyn Future<Output = PulseResult<T>> + Send + 'static>>;

/// Server-issued, bounded cursor. Its contents are intentionally not part of
/// the federation contract.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpaqueCursor(String);

impl OpaqueCursor {
    /// Validates a server-issued URL-safe cursor.
    ///
    /// # Errors
    ///
    /// Returns invalid input for empty, oversized, or non-URL-safe values.
    pub fn new(value: impl Into<String>) -> PulseResult<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_CURSOR_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
            })
        {
            return Err(PulseError::invalid_input(
                "Pulse federation cursor is not bounded URL-safe text",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Machine that originally produced a row and the machines through which it
/// has passed. Direct export is exactly one hop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PulseOrigin {
    pub machine: MachineName,
    pub path: Vec<MachineName>,
}

impl PulseOrigin {
    #[must_use]
    pub fn local(machine: MachineName) -> Self {
        Self {
            machine: machine.clone(),
            path: vec![machine],
        }
    }

    fn validate_direct(&self, expected: &MachineName) -> PulseResult<()> {
        if self.path.len() != 1
            || self.path.len() > MAX_ORIGIN_HOPS
            || self.machine != *expected
            || self.path.first() != Some(expected)
        {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "a Pulse peer attempted to re-export mirrored data",
            ));
        }
        Ok(())
    }
}

/// Account-scoped Pulse rows allowed over pull federation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "row", rename_all = "snake_case")]
pub enum FederatedPulseRow {
    Machine(Machine),
    Profile(Profile),
    Usage(UsageSnapshot),
    Context(ContextSession),
    Token(TokenGrain),
}

impl FederatedPulseRow {
    #[must_use]
    pub const fn account_id(&self) -> AccountId {
        match self {
            Self::Machine(row) => row.account_id,
            Self::Profile(row) => row.account_id,
            Self::Usage(row) => row.account_id,
            Self::Context(row) => row.account_id,
            Self::Token(row) => row.account_id,
        }
    }
}

/// One idempotently applicable row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FederatedRecord {
    pub key: String,
    pub origin: PulseOrigin,
    pub row: FederatedPulseRow,
}

impl FederatedRecord {
    pub(crate) fn validate(
        &self,
        account_id: AccountId,
        expected: &MachineName,
    ) -> PulseResult<()> {
        if self.key.is_empty()
            || self.key.len() > 192
            || !self.key.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            })
        {
            return Err(PulseError::invalid_input(
                "Pulse federation record key is invalid",
            ));
        }
        self.origin.validate_direct(expected)?;
        if self.row.account_id() != account_id {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "Pulse federation row crossed its requested account boundary",
            ));
        }
        match &self.row {
            FederatedPulseRow::Machine(row) if row.name != *expected => {
                return Err(origin_mismatch());
            }
            FederatedPulseRow::Profile(row) => {
                if row.origin != ProfileOrigin::Reported || row.validate().is_err() {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse federation profile exposed local-only metadata",
                    ));
                }
            }
            FederatedPulseRow::Usage(row) if row.machine != *expected => {
                return Err(origin_mismatch());
            }
            FederatedPulseRow::Context(row) if row.machine != *expected => {
                return Err(origin_mismatch());
            }
            FederatedPulseRow::Token(row) if row.machine != *expected => {
                return Err(origin_mismatch());
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn fingerprint(&self) -> PulseResult<String> {
        let encoded = serde_json::to_vec(&self.row).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Internal,
                "Pulse federation row could not be fingerprinted",
            )
        })?;
        Ok(format!("{:x}", Sha256::digest(encoded)))
    }

    pub(crate) const fn apply_priority(&self) -> u8 {
        match &self.row {
            FederatedPulseRow::Machine(_) => 0,
            FederatedPulseRow::Profile(_) => 1,
            FederatedPulseRow::Usage(_) => 2,
            FederatedPulseRow::Context(_) => 3,
            FederatedPulseRow::Token(_) => 4,
        }
    }
}

fn origin_mismatch() -> PulseError {
    PulseError::new(
        PulseErrorKind::Conflict,
        "Pulse federation row machine did not match its authenticated origin",
    )
}

/// One bounded page produced by a direct peer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FederationPage {
    pub version: u16,
    pub source_machine: MachineName,
    pub records: Vec<FederatedRecord>,
    pub next_cursor: Option<OpaqueCursor>,
}

impl FederationPage {
    fn validate(
        &self,
        account_id: AccountId,
        expected: &MachineName,
        requested_limit: u16,
    ) -> PulseResult<()> {
        if self.version != FEDERATION_VERSION || self.source_machine != *expected {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "Pulse federation peer identity or version did not match",
            ));
        }
        let limit = usize::from(requested_limit.min(MAX_PAGE_ROWS));
        if self.records.len() > limit {
            return Err(PulseError::new(
                PulseErrorKind::Upstream,
                "Pulse federation page exceeded its row bound",
            ));
        }
        for record in &self.records {
            record.validate(account_id, expected)?;
        }
        if self.records.is_empty() && self.next_cursor.is_some() {
            return Err(PulseError::new(
                PulseErrorKind::Upstream,
                "Pulse federation peer returned a non-advancing cursor",
            ));
        }
        Ok(())
    }
}

/// Per-machine resume state. Persisting this value is sufficient to continue
/// after a coordinator restart; consumers remain responsible for applying the
/// stable record key idempotently.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederationState {
    pub cursor: Option<OpaqueCursor>,
    pub generation: u64,
    pub pages_applied: u64,
    pub records_applied: u64,
    pub complete: bool,
}

/// Pull request issued to one peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FederationRequest {
    pub account_id: AccountId,
    pub machine: MachineName,
    pub cursor: Option<OpaqueCursor>,
    pub limit: u16,
}

/// Transport boundary shared by the real atmux remote and deterministic tests.
pub trait FederationTransport: Send + Sync + 'static {
    fn fetch_page(&self, request: FederationRequest) -> FederationFuture<FederationPage>;
}

/// Idempotent sink for one validated page. The sink must commit all records or
/// none; the cursor advances only after this call succeeds.
pub trait FederationConsumer: Send + Sync + 'static {
    /// Loads the durable state and begins a new generation after a completed
    /// scan. Incomplete scans retain their committed cursor across restarts.
    fn begin_sync(
        &self,
        account_id: AccountId,
        machine: MachineName,
    ) -> FederationFuture<FederationState>;

    fn apply_page(
        &self,
        account_id: AccountId,
        machine: MachineName,
        expected_cursor: Option<OpaqueCursor>,
        next_cursor: Option<OpaqueCursor>,
        records: Vec<FederatedRecord>,
    ) -> FederationFuture<FederationState>;
}

/// Transactional federation consumer backed by the configured Pulse store.
pub struct StoreFederationConsumer {
    store: Arc<dyn Store>,
    invalidations: Option<super::invalidation::PulseInvalidationHub>,
}

impl StoreFederationConsumer {
    #[must_use]
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self {
            store,
            invalidations: None,
        }
    }

    #[must_use]
    pub fn with_invalidations(
        mut self,
        invalidations: super::invalidation::PulseInvalidationHub,
    ) -> Self {
        self.invalidations = Some(invalidations);
        self
    }
}

impl FederationConsumer for StoreFederationConsumer {
    fn begin_sync(
        &self,
        account_id: AccountId,
        machine: MachineName,
    ) -> FederationFuture<FederationState> {
        self.store.begin_federation_sync(account_id, machine)
    }

    fn apply_page(
        &self,
        account_id: AccountId,
        machine: MachineName,
        expected_cursor: Option<OpaqueCursor>,
        next_cursor: Option<OpaqueCursor>,
        records: Vec<FederatedRecord>,
    ) -> FederationFuture<FederationState> {
        let store = Arc::clone(&self.store);
        let invalidations = self.invalidations.clone();
        Box::pin(async move {
            let state = store
                .apply_federation_page(account_id, machine, expected_cursor, next_cursor, records)
                .await?;
            if let Some(invalidations) = invalidations {
                let _ = invalidations.publish(account_id);
            }
            Ok(state)
        })
    }
}

/// Existing web Host/node-auth policy has authorized a direct pull request.
#[derive(Clone, Copy, Debug)]
pub struct VerifiedFederationBoundary(());

impl VerifiedFederationBoundary {
    #[must_use]
    pub(crate) const fn after_host_and_node_auth_checks() -> Self {
        Self(())
    }
}

/// Origin-free source row. The exporter attaches the local machine and a
/// single-hop path, so query code cannot accidentally mirror a remote row.
#[derive(Clone, Debug)]
pub struct LocalFederationRecord {
    pub key: String,
    pub row: FederatedPulseRow,
    pub(crate) position: FederationExportPosition,
}

/// Stable SQL keyset position. Fields are natural-key components for one row
/// class; phase ordering keeps prerequisite machine/profile rows first.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FederationExportPosition {
    pub(crate) phase: u8,
    pub(crate) values: Vec<String>,
}

impl FederationExportPosition {
    pub(crate) fn new(phase: u8, values: Vec<String>) -> PulseResult<Self> {
        if phase > 4
            || values.is_empty()
            || values.len() > 8
            || values.iter().any(|value| value.len() > 512)
        {
            return Err(PulseError::new(
                PulseErrorKind::Storage,
                "Pulse federation export position is invalid",
            ));
        }
        Ok(Self { phase, values })
    }
}

impl LocalFederationRecord {
    pub(crate) fn new(
        position: FederationExportPosition,
        row: FederatedPulseRow,
    ) -> PulseResult<Self> {
        let encoded = serde_json::to_vec(&position).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Internal,
                "Pulse federation key could not be encoded",
            )
        })?;
        let mut digest = Sha256::new();
        digest.update(encoded);
        if !matches!(&row, FederatedPulseRow::Usage(_)) {
            digest.update(serde_json::to_vec(&row).map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Internal,
                    "Pulse federation row version could not be encoded",
                )
            })?);
        }
        let prefix = char::from(b'a' + position.phase);
        Ok(Self {
            key: format!("{prefix}/{:x}", digest.finalize()),
            row,
            position,
        })
    }
}

/// One bounded local source page.
#[derive(Clone, Debug)]
pub struct LocalFederationPage {
    pub records: Vec<LocalFederationRecord>,
    pub next_cursor: Option<OpaqueCursor>,
}

/// Account-scoped, SQL-bounded local query seam used by the authenticated
/// federation route.
pub trait LocalFederationSource: Send + Sync + 'static {
    fn local_page(&self, request: FederationRequest) -> FederationFuture<LocalFederationPage>;
}

/// Store-backed direct source that can expose only rows produced by this node.
///
/// Account-global Gemini rows and control-plane alert/pricing rows are omitted
/// because their current models have no originating machine. Re-exporting
/// those rows could create a mirror loop. They remain available through the
/// account-scoped REST/MCP surfaces.
pub struct StoreFederationSource {
    store: Arc<dyn Store>,
    accounts: Arc<BTreeSet<AccountId>>,
    local_machine: MachineName,
}

impl StoreFederationSource {
    #[must_use]
    pub fn new(store: Arc<dyn Store>, accounts: &[AccountId], local_machine: MachineName) -> Self {
        Self {
            store,
            accounts: Arc::new(accounts.iter().copied().collect()),
            local_machine,
        }
    }
}

impl LocalFederationSource for StoreFederationSource {
    fn local_page(&self, request: FederationRequest) -> FederationFuture<LocalFederationPage> {
        let store = Arc::clone(&self.store);
        let accounts = Arc::clone(&self.accounts);
        let local_machine = self.local_machine.clone();
        Box::pin(async move {
            if !accounts.contains(&request.account_id) || request.machine != local_machine {
                return Err(PulseError::new(
                    PulseErrorKind::NotFound,
                    "Pulse federation account was not found",
                ));
            }
            let after = parse_export_cursor(request.cursor.as_ref())?;
            let fetch_limit = usize::from(request.limit).saturating_add(1);
            let mut page = store
                .local_federation_page(request.account_id, local_machine, after, fetch_limit)
                .await?;
            let has_more = page.len() > usize::from(request.limit);
            page.truncate(usize::from(request.limit));
            let next_cursor = if has_more {
                page.last()
                    .map(|record| export_cursor(&record.position))
                    .transpose()?
            } else {
                None
            };
            Ok(LocalFederationPage {
                records: page,
                next_cursor,
            })
        })
    }
}

fn export_cursor(position: &FederationExportPosition) -> PulseResult<OpaqueCursor> {
    let encoded = serde_json::to_vec(position).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Internal,
            "Pulse federation cursor could not be encoded",
        )
    })?;
    OpaqueCursor::new(format!("v2.{}", URL_SAFE_NO_PAD.encode(encoded)))
}

fn parse_export_cursor(
    cursor: Option<&OpaqueCursor>,
) -> PulseResult<Option<FederationExportPosition>> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let Some(value) = cursor.as_str().strip_prefix("v2.") else {
        return Err(PulseError::invalid_input(
            "Pulse federation cursor version is invalid",
        ));
    };
    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| PulseError::invalid_input("Pulse federation cursor is invalid"))?;
    let position: FederationExportPosition = serde_json::from_slice(&decoded)
        .map_err(|_| PulseError::invalid_input("Pulse federation cursor is invalid"))?;
    FederationExportPosition::new(position.phase, position.values).map(Some)
}

/// Server-side direct exporter. This is intentionally separate from REST so
/// the route must supply proof that existing atmux node authentication ran.
pub struct DirectFederationExporter {
    local_machine: MachineName,
    source: Arc<dyn LocalFederationSource>,
}

impl DirectFederationExporter {
    #[must_use]
    pub fn new(local_machine: MachineName, source: Arc<dyn LocalFederationSource>) -> Self {
        Self {
            local_machine,
            source,
        }
    }

    /// Produces a direct, non-mirrorable federation page.
    ///
    /// # Errors
    ///
    /// Returns invalid input for a bad page limit and rejects source rows that
    /// cross account, machine, or local-profile security boundaries.
    pub async fn page(
        &self,
        boundary: VerifiedFederationBoundary,
        account_id: AccountId,
        cursor: Option<OpaqueCursor>,
        limit: u16,
    ) -> PulseResult<FederationPage> {
        let VerifiedFederationBoundary(()) = boundary;
        if limit == 0 || limit > MAX_PAGE_ROWS {
            return Err(PulseError::invalid_input(
                "Pulse federation page limit is out of bounds",
            ));
        }
        let local = self
            .source
            .local_page(FederationRequest {
                account_id,
                machine: self.local_machine.clone(),
                cursor,
                limit,
            })
            .await?;
        if local.records.len() > usize::from(limit) {
            return Err(PulseError::new(
                PulseErrorKind::Storage,
                "Pulse federation source exceeded its row bound",
            ));
        }
        let origin = PulseOrigin::local(self.local_machine.clone());
        let page = FederationPage {
            version: FEDERATION_VERSION,
            source_machine: self.local_machine.clone(),
            records: local
                .records
                .into_iter()
                .map(|record| FederatedRecord {
                    key: record.key,
                    origin: origin.clone(),
                    row: record.row,
                })
                .collect(),
            next_cursor: local.next_cursor,
        };
        page.validate(account_id, &self.local_machine, limit)?;
        Ok(page)
    }
}

/// Pull transport backed by the existing mTLS/token-aware node client.
pub struct AtmuxPullTransport {
    remotes: BTreeMap<MachineName, Arc<RemoteMachine>>,
}

impl AtmuxPullTransport {
    /// Builds a transport after checking map identity matches each remote.
    ///
    /// # Errors
    ///
    /// Returns invalid input for duplicate/invalid machine identifiers.
    pub fn new(remotes: Vec<Arc<RemoteMachine>>) -> PulseResult<Self> {
        if remotes.len() > MAX_CONFIGURED_PEERS {
            return Err(PulseError::configuration(format!(
                "Pulse federation cannot exceed {MAX_CONFIGURED_PEERS} configured peers"
            )));
        }
        let mut indexed = BTreeMap::new();
        for remote in remotes {
            if !remote.is_authenticated() {
                return Err(PulseError::configuration(format!(
                    "Pulse federation peer {} has no node token",
                    remote.id
                )));
            }
            let machine = MachineName::new(remote.id.clone())?;
            if indexed.insert(machine, remote).is_some() {
                return Err(PulseError::invalid_input(
                    "Pulse federation contains a duplicate machine",
                ));
            }
        }
        Ok(Self { remotes: indexed })
    }
}

impl FederationTransport for AtmuxPullTransport {
    fn fetch_page(&self, request: FederationRequest) -> FederationFuture<FederationPage> {
        let remote = self.remotes.get(&request.machine).cloned();
        Box::pin(async move {
            let remote = remote.ok_or_else(|| {
                PulseError::new(PulseErrorKind::NotFound, "Pulse peer is not configured")
            })?;
            let limit = request.limit.clamp(1, MAX_PAGE_ROWS);
            let mut path = format!(
                "/api/v1/pulse/accounts/{}/federation?limit={limit}",
                request.account_id.get()
            );
            if let Some(cursor) = request.cursor {
                path.push_str("&cursor=");
                path.push_str(cursor.as_str());
            }
            remote
                .get_json(&path)
                .await
                .map_err(|_| PulseError::new(PulseErrorKind::Offline, "Pulse peer is unavailable"))
        })
    }
}

/// Pulls bounded pages from one or more peers while keeping failures isolated.
pub struct PulseFederation {
    transport: Arc<dyn FederationTransport>,
    consumer: Arc<dyn FederationConsumer>,
    page_rows: u16,
    max_pages_per_sync: usize,
}

impl PulseFederation {
    #[must_use]
    pub fn new(
        transport: Arc<dyn FederationTransport>,
        consumer: Arc<dyn FederationConsumer>,
    ) -> Self {
        Self {
            transport,
            consumer,
            page_rows: DEFAULT_PAGE_ROWS,
            max_pages_per_sync: MAX_PAGES_PER_SYNC,
        }
    }

    #[cfg(test)]
    fn with_bounds(mut self, page_rows: u16, max_pages_per_sync: usize) -> Self {
        self.page_rows = page_rows.clamp(1, MAX_PAGE_ROWS);
        self.max_pages_per_sync = max_pages_per_sync.max(1);
        self
    }

    /// Pulls one machine from its last committed cursor.
    ///
    /// # Errors
    ///
    /// Returns only this machine's transport, validation, or consumer error.
    pub async fn sync_machine(
        &self,
        account_id: AccountId,
        machine: MachineName,
    ) -> PulseResult<FederationState> {
        let mut state = self
            .consumer
            .begin_sync(account_id, machine.clone())
            .await?;
        for _ in 0..self.max_pages_per_sync {
            let prior_cursor = state.cursor.clone();
            let page = self
                .transport
                .fetch_page(FederationRequest {
                    account_id,
                    machine: machine.clone(),
                    cursor: prior_cursor.clone(),
                    limit: self.page_rows,
                })
                .await?;
            page.validate(account_id, &machine, self.page_rows)?;
            validate_cursor_progress(
                prior_cursor.as_ref(),
                page.next_cursor.as_ref(),
                &page.records,
            )?;
            let next_cursor = page.next_cursor;
            state = self
                .consumer
                .apply_page(
                    account_id,
                    machine.clone(),
                    prior_cursor.clone(),
                    next_cursor,
                    page.records,
                )
                .await?;
            if state.complete {
                return Ok(state);
            }
        }
        Ok(state)
    }

    /// Synchronizes peers concurrently. One offline or hostile peer is returned
    /// as an error entry without cancelling healthy peers.
    pub async fn sync_all(
        self: Arc<Self>,
        account_id: AccountId,
        machines: Vec<MachineName>,
    ) -> BTreeMap<MachineName, PulseResult<FederationState>> {
        let mut tasks = JoinSet::new();
        let concurrency = Arc::new(Semaphore::new(MAX_CONCURRENT_PEERS));
        let mut results = BTreeMap::new();
        for (index, machine) in machines.into_iter().enumerate() {
            if index >= MAX_CONFIGURED_PEERS {
                results.insert(
                    machine,
                    Err(PulseError::configuration(
                        "Pulse federation peer list exceeded its work bound",
                    )),
                );
                continue;
            }
            let federation = Arc::clone(&self);
            let concurrency = Arc::clone(&concurrency);
            tasks.spawn(async move {
                let Ok(_permit) = concurrency.acquire_owned().await else {
                    return (
                        machine,
                        Err(PulseError::new(
                            PulseErrorKind::Internal,
                            "Pulse federation concurrency guard closed",
                        )),
                    );
                };
                let result = federation.sync_machine(account_id, machine.clone()).await;
                (machine, result)
            });
        }
        while let Some(joined) = tasks.join_next().await {
            if let Ok((machine, result)) = joined {
                results.insert(machine, result);
            }
        }
        results
    }
}

fn validate_cursor_progress(
    prior: Option<&OpaqueCursor>,
    next: Option<&OpaqueCursor>,
    records: &[FederatedRecord],
) -> PulseResult<()> {
    let prior_key = parse_export_cursor(prior)?;
    let next_key = parse_export_cursor(next)?;
    if next_key
        .as_ref()
        .is_some_and(|next| prior_key.as_ref().is_some_and(|prior| next <= prior))
    {
        return Err(PulseError::new(
            PulseErrorKind::Upstream,
            "Pulse federation cursor did not strictly advance",
        ));
    }
    if records.is_empty() && next_key.is_some() {
        return Err(PulseError::new(
            PulseErrorKind::Upstream,
            "Pulse federation empty page returned an advancing cursor",
        ));
    }
    Ok(())
}

/// One bounded, process-owned periodic pull task.
pub struct FederationPullLifecycle {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl FederationPullLifecycle {
    /// Starts one immediate scan followed by bounded periodic resyncs.
    ///
    /// Each peer is isolated by [`PulseFederation::sync_all`]; an offline peer
    /// never cancels healthy peer work.
    #[must_use]
    pub fn start(
        federation: Arc<PulseFederation>,
        accounts: Arc<[AccountId]>,
        machines: Arc<[MachineName]>,
        interval: Duration,
    ) -> Self {
        let interval = interval.max(Duration::from_secs(30));
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                for account_id in accounts.iter().copied() {
                    let results = Arc::clone(&federation)
                        .sync_all(account_id, machines.to_vec())
                        .await;
                    for (machine, result) in results {
                        if let Err(error) = result {
                            eprintln!(
                                "atmux Pulse federation: account={} peer={} status={:?}",
                                account_id.get(),
                                machine,
                                error.kind()
                            );
                        }
                    }
                    if *shutdown_rx.borrow() {
                        return;
                    }
                }
                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            return;
                        }
                    }
                }
            }
        });
        Self { shutdown, task }
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        let _ = (&mut self.task).await;
    }
}

impl Drop for FederationPullLifecycle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::pulse::{
        Account, CollectionOutcome, Instant, Percent, ProfileName, QuotaWindow, QuotaWindowKind,
        Vendor, store::SqliteStore,
    };

    type PageKey = (MachineName, Option<OpaqueCursor>);
    type PageResult = PulseResult<FederationPage>;

    struct FakeTransport {
        pages: Mutex<BTreeMap<PageKey, PageResult>>,
    }

    struct FakeLocalSource {
        page: Mutex<Option<PulseResult<LocalFederationPage>>>,
    }

    impl LocalFederationSource for FakeLocalSource {
        fn local_page(&self, _request: FederationRequest) -> FederationFuture<LocalFederationPage> {
            let page = self
                .page
                .lock()
                .expect("page lock")
                .take()
                .expect("one page");
            Box::pin(async move { page })
        }
    }

    impl FederationTransport for FakeTransport {
        fn fetch_page(&self, request: FederationRequest) -> FederationFuture<FederationPage> {
            let value = self
                .pages
                .lock()
                .expect("pages lock")
                .remove(&(request.machine, request.cursor))
                .unwrap_or_else(|| {
                    Err(PulseError::new(
                        PulseErrorKind::Offline,
                        "fixture peer unavailable",
                    ))
                });
            Box::pin(async move { value })
        }
    }

    #[derive(Default)]
    struct FakeConsumer {
        keys: Mutex<Vec<String>>,
        states: Mutex<BTreeMap<(AccountId, MachineName), FederationState>>,
    }

    impl FederationConsumer for FakeConsumer {
        fn begin_sync(
            &self,
            account_id: AccountId,
            machine: MachineName,
        ) -> FederationFuture<FederationState> {
            let mut states = self.states.lock().expect("states lock");
            let state = states.entry((account_id, machine)).or_default();
            if state.complete {
                state.cursor = None;
                state.generation = state.generation.saturating_add(1);
                state.complete = false;
            }
            let state = state.clone();
            Box::pin(async move { Ok(state) })
        }

        fn apply_page(
            &self,
            account_id: AccountId,
            machine: MachineName,
            expected_cursor: Option<OpaqueCursor>,
            next_cursor: Option<OpaqueCursor>,
            records: Vec<FederatedRecord>,
        ) -> FederationFuture<FederationState> {
            let record_count = u64::try_from(records.len()).unwrap_or(u64::MAX);
            self.keys
                .lock()
                .expect("keys lock")
                .extend(records.into_iter().map(|record| record.key));
            let mut states = self.states.lock().expect("states lock");
            let state = states.entry((account_id, machine)).or_default();
            assert_eq!(state.cursor, expected_cursor);
            state.cursor = next_cursor;
            state.pages_applied = state.pages_applied.saturating_add(1);
            state.records_applied = state.records_applied.saturating_add(record_count);
            state.complete = state.cursor.is_none();
            let state = state.clone();
            Box::pin(async move { Ok(state) })
        }
    }

    fn machine(value: &str) -> MachineName {
        MachineName::new(value).expect("machine")
    }

    fn account() -> AccountId {
        AccountId::new(7).expect("account")
    }

    fn private_sqlite_path(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "atmux-pulse-federation-{label}-{}-{}",
            std::process::id(),
            Instant::now().epoch_millis()
        ));
        std::fs::create_dir(&directory).expect("create private SQLite test directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .expect("secure SQLite test directory");
        }
        let path = directory.join("pulse.sqlite3");
        (directory, path)
    }

    fn remove_sqlite_test_files(directory: &std::path::Path, path: &std::path::Path) {
        for candidate in [
            path.to_path_buf(),
            std::path::PathBuf::from(format!("{}-wal", path.display())),
            std::path::PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = std::fs::remove_file(candidate);
        }
        let _ = std::fs::remove_dir(directory);
    }

    #[tokio::test]
    async fn store_consumer_invalidates_only_after_successful_account_scoped_apply() {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open(":memory:").await.expect("store"));
        let one = account();
        let two = AccountId::new(8).expect("other account");
        for (id, identity) in [(one, "one@example.test"), (two, "two@example.test")] {
            store
                .upsert_account(Account {
                    id,
                    identity: identity.to_owned(),
                    display_name: None,
                })
                .await
                .expect("account");
        }
        let invalidations = super::super::invalidation::PulseInvalidationHub::new(&[one, two]);
        let mut one_events = invalidations.subscribe(one).expect("one subscription");
        let mut two_events = invalidations.subscribe(two).expect("two subscription");
        let consumer = StoreFederationConsumer::new(store).with_invalidations(invalidations);
        let remote = machine("max");
        consumer
            .begin_sync(one, remote.clone())
            .await
            .expect("begin sync");
        consumer
            .apply_page(one, remote.clone(), None, None, Vec::new())
            .await
            .expect("successful page");
        one_events.receiver.changed().await.expect("invalidation");
        assert_eq!(*one_events.receiver.borrow_and_update(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), two_events.receiver.changed())
                .await
                .is_err(),
            "account one federation must not invalidate account two"
        );

        consumer
            .begin_sync(one, remote.clone())
            .await
            .expect("begin next generation");
        let failed = consumer
            .apply_page(
                one,
                remote,
                Some(OpaqueCursor::new("unexpected").expect("cursor")),
                None,
                Vec::new(),
            )
            .await;
        assert!(failed.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(25), one_events.receiver.changed())
                .await
                .is_err(),
            "failed federation pages must not publish invalidations"
        );
    }

    fn bulk_insert_usage(
        path: &std::path::Path,
        local: &MachineName,
        remote: &MachineName,
        local_rows: usize,
        mirrored_rows: usize,
    ) {
        let mut connection = rusqlite::Connection::open(path).expect("bulk database");
        let transaction = connection.transaction().expect("bulk transaction");
        let vendor = serde_json::to_string(&Vendor::AnthropicOauth).expect("vendor");
        let outcome = serde_json::to_string(&CollectionOutcome::Success).expect("outcome");
        let kind = serde_json::to_string(&QuotaWindowKind::FiveHour).expect("kind");
        for (machine, count, base) in [
            (local, local_rows, 10_000_i64),
            (remote, mirrored_rows, 1_000_000_i64),
        ] {
            for index in 0..count {
                let index = i64::try_from(index).expect("index");
                transaction
                    .execute(
                        "INSERT INTO usage_snapshots (account_id,profile,machine,vendor_json, \
                         outcome_json,polled_at_ms,reporter_version) \
                         VALUES (?1,'claude',?2,?3,?4,?5,'bulk')",
                        rusqlite::params![
                            account().get(),
                            machine.as_str(),
                            vendor,
                            outcome,
                            base + index
                        ],
                    )
                    .expect("snapshot");
                let id = transaction.last_insert_rowid();
                transaction
                    .execute(
                        "INSERT INTO usage_windows \
                         (snapshot_id,kind_json,used_percent,resets_at_ms,accepted) \
                         VALUES (?1,?2,10.0,2000000,1)",
                        rusqlite::params![id, kind],
                    )
                    .expect("window");
            }
        }
        transaction.commit().expect("commit bulk usage");
    }

    fn record(source: &str, path: &[&str], key: &str) -> FederatedRecord {
        let source = machine(source);
        FederatedRecord {
            key: key.to_owned(),
            origin: PulseOrigin {
                machine: source,
                path: path.iter().map(|value| machine(value)).collect(),
            },
            row: FederatedPulseRow::Usage(UsageSnapshot {
                account_id: account(),
                profile: ProfileName::new("claude").expect("profile"),
                machine: machine(path[0]),
                vendor: Vendor::AnthropicOauth,
                windows: vec![QuotaWindow {
                    kind: QuotaWindowKind::FiveHour,
                    used_percent: Percent::new(10.0).expect("percent"),
                    resets_at: Instant::from_epoch_millis(100_000).expect("instant"),
                }],
                outcome: crate::pulse::CollectionOutcome::Success,
                polled_at: Instant::from_epoch_millis(10_000).expect("instant"),
                reporter_version: Some("test".to_owned()),
            }),
        }
    }

    #[tokio::test]
    async fn resumes_from_committed_cursor_and_applies_pages_once() {
        let first = export_cursor(
            &FederationExportPosition::new(2, vec!["1".to_owned()]).expect("position"),
        )
        .expect("cursor");
        let remote = machine("midnight");
        let pages = BTreeMap::from([
            (
                (remote.clone(), None),
                Ok(FederationPage {
                    version: FEDERATION_VERSION,
                    source_machine: remote.clone(),
                    records: vec![record("midnight", &["midnight"], "row:1")],
                    next_cursor: Some(first.clone()),
                }),
            ),
            (
                (remote.clone(), Some(first)),
                Ok(FederationPage {
                    version: FEDERATION_VERSION,
                    source_machine: remote.clone(),
                    records: vec![record("midnight", &["midnight"], "row:2")],
                    next_cursor: None,
                }),
            ),
        ]);
        let consumer = Arc::new(FakeConsumer::default());
        let federation = PulseFederation::new(
            Arc::new(FakeTransport {
                pages: Mutex::new(pages),
            }),
            consumer.clone(),
        )
        .with_bounds(1, 8);
        let state = federation
            .sync_machine(account(), remote)
            .await
            .expect("sync");
        assert!(state.complete);
        assert_eq!(state.pages_applied, 2);
        assert_eq!(*consumer.keys.lock().expect("keys"), vec!["row:1", "row:2"]);
    }

    #[tokio::test]
    async fn three_nodes_isolate_offline_and_refuse_mirrored_reexport() {
        let max = machine("max");
        let midnight = machine("midnight");
        let tron = machine("tron");
        let pages = BTreeMap::from([
            (
                (max.clone(), None),
                Ok(FederationPage {
                    version: FEDERATION_VERSION,
                    source_machine: max.clone(),
                    records: vec![record("max", &["max"], "max:1")],
                    next_cursor: None,
                }),
            ),
            (
                (midnight.clone(), None),
                Ok(FederationPage {
                    version: FEDERATION_VERSION,
                    source_machine: midnight.clone(),
                    records: vec![record("tron", &["tron", "midnight"], "loop:1")],
                    next_cursor: None,
                }),
            ),
        ]);
        let federation = Arc::new(PulseFederation::new(
            Arc::new(FakeTransport {
                pages: Mutex::new(pages),
            }),
            Arc::new(FakeConsumer::default()),
        ));
        let results = federation
            .sync_all(account(), vec![max.clone(), midnight.clone(), tron.clone()])
            .await;
        assert!(results[&max].as_ref().expect("healthy max").complete);
        assert_eq!(
            results[&midnight]
                .as_ref()
                .expect_err("loop rejected")
                .kind(),
            PulseErrorKind::Conflict
        );
        assert_eq!(
            results[&tron]
                .as_ref()
                .expect_err("offline isolated")
                .kind(),
            PulseErrorKind::Offline
        );
    }

    #[test]
    fn cursor_and_page_bounds_fail_closed() {
        assert!(OpaqueCursor::new("").is_err());
        assert!(OpaqueCursor::new("x".repeat(MAX_CURSOR_BYTES + 1)).is_err());
        assert!(OpaqueCursor::new("query=value").is_err());
    }

    #[test]
    fn pull_transport_rejects_unauthenticated_configured_peers() {
        let remote = RemoteMachine::from_config(&crate::config::MachineConfig {
            id: "max".to_owned(),
            label: None,
            url: "http://127.0.0.1:7345".to_owned(),
            token_env: None,
            token_file: None,
        })
        .expect("remote");
        let Err(error) = AtmuxPullTransport::new(vec![Arc::new(remote)]) else {
            panic!("tokenless peer must be rejected");
        };
        assert_eq!(error.kind(), PulseErrorKind::Configuration);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn store_exporter_is_account_allowlisted_paged_and_local_only() {
        let (directory, path) = private_sqlite_path("exporter");
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&path).await.expect("store"));
        let local = machine("midnight");
        let remote = machine("max");
        store
            .upsert_account(Account {
                id: account(),
                identity: "operator@example.test".to_owned(),
                display_name: None,
            })
            .await
            .expect("account");
        for name in [local.clone(), remote.clone()] {
            store
                .upsert_machine(Machine {
                    account_id: account(),
                    name,
                    first_seen: Instant::from_epoch_millis(1).expect("instant"),
                    last_seen: Instant::from_epoch_millis(2).expect("instant"),
                })
                .await
                .expect("machine");
        }
        store
            .upsert_profile(Profile {
                account_id: account(),
                name: ProfileName::new("claude").expect("profile"),
                vendor: Vendor::AnthropicOauth,
                config_dir: Some(std::path::PathBuf::from("/private/claude")),
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
        for (name, at) in [(local.clone(), 10_000), (remote, 20_000)] {
            store
                .append_usage_snapshot(UsageSnapshot {
                    account_id: account(),
                    profile: ProfileName::new("claude").expect("profile"),
                    machine: name,
                    vendor: Vendor::AnthropicOauth,
                    windows: vec![QuotaWindow {
                        kind: QuotaWindowKind::FiveHour,
                        used_percent: Percent::new(10.0).expect("percent"),
                        resets_at: Instant::from_epoch_millis(100_000).expect("instant"),
                    }],
                    outcome: CollectionOutcome::Success,
                    polled_at: Instant::from_epoch_millis(at).expect("instant"),
                    reporter_version: Some("fixture".to_owned()),
                })
                .await
                .expect("snapshot");
        }
        let exporter = DirectFederationExporter::new(
            local.clone(),
            Arc::new(StoreFederationSource::new(
                Arc::clone(&store),
                &[account()],
                local.clone(),
            )),
        );
        let first = exporter
            .page(
                VerifiedFederationBoundary::after_host_and_node_auth_checks(),
                account(),
                None,
                2,
            )
            .await
            .expect("first page");
        assert_eq!(first.records.len(), 2);
        assert!(first.next_cursor.is_some());
        assert!(first.records.iter().all(|record| {
            record.origin == PulseOrigin::local(local.clone())
                && !matches!(
                    &record.row,
                    FederatedPulseRow::Usage(snapshot) if snapshot.machine != local
                )
        }));
        // Insert a source row that sorts before the committed last-key while
        // this scan is in progress. Offset pagination would shift and repeat
        // the old profile; the stable cursor continues strictly after it.
        let committed_key = first.records.last().expect("last record").key.clone();
        let earlier_profile = Profile {
            account_id: account(),
            name: ProfileName::new("aardvark").expect("profile"),
            vendor: Vendor::AnthropicOauth,
            config_dir: None,
            poll_interval_minutes: 15,
            monthly_budget_usd: None,
            api_key_env: None,
            api_key_file: None,
            refresh: RefreshPolicy::InMemory,
            hidden: false,
            origin: ProfileOrigin::Local,
        };
        store
            .upsert_profile(earlier_profile.clone())
            .await
            .expect("insert mutable source row");
        let second = exporter
            .page(
                VerifiedFederationBoundary::after_host_and_node_auth_checks(),
                account(),
                first.next_cursor,
                2,
            )
            .await
            .expect("second page");
        assert!(!second.records.is_empty());
        assert!(second.next_cursor.is_none());
        assert!(
            second
                .records
                .iter()
                .all(|record| record.key != committed_key),
            "a mutable page must never repeat the committed last-key record"
        );
        let resync = exporter
            .page(
                VerifiedFederationBoundary::after_host_and_node_auth_checks(),
                account(),
                None,
                10,
            )
            .await
            .expect("full resync");
        assert!(resync.records.iter().any(|record| {
            matches!(
                &record.row,
                FederatedPulseRow::Profile(profile) if profile.name == earlier_profile.name
            )
        }));
        assert!(
            exporter
                .page(
                    VerifiedFederationBoundary::after_host_and_node_auth_checks(),
                    AccountId::new(8).expect("other account"),
                    None,
                    2,
                )
                .await
                .is_err()
        );
        drop(store);
        remove_sqlite_test_files(&directory, &path);
    }

    #[tokio::test]
    async fn sql_keyset_export_pages_beyond_ten_thousand_without_mirror_starvation() {
        const LOCAL_ROWS: usize = 10_050;
        const MIRRORED_ROWS: usize = 15_000;
        let (directory, path) = private_sqlite_path("large");
        let store = SqliteStore::open(&path).await.expect("store");
        let local = machine("midnight");
        let remote = machine("max");
        store
            .upsert_account(Account {
                id: account(),
                identity: "large@example.test".to_owned(),
                display_name: None,
            })
            .await
            .expect("account");
        for name in [local.clone(), remote.clone()] {
            store
                .upsert_machine(Machine {
                    account_id: account(),
                    name,
                    first_seen: Instant::from_epoch_millis(1).expect("instant"),
                    last_seen: Instant::from_epoch_millis(2).expect("instant"),
                })
                .await
                .expect("machine");
        }
        store
            .upsert_profile(Profile {
                account_id: account(),
                name: ProfileName::new("claude").expect("profile"),
                vendor: Vendor::AnthropicOauth,
                config_dir: Some(std::path::PathBuf::from("/private/claude")),
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
        drop(store);
        // Mirrored rows are inserted last, so the old DESC/LIMIT-then-filter
        // implementation saw only mirrored data and starved the local source.
        bulk_insert_usage(&path, &local, &remote, LOCAL_ROWS, MIRRORED_ROWS);

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&path).await.expect("reopen"));
        let exporter = DirectFederationExporter::new(
            local.clone(),
            Arc::new(StoreFederationSource::new(
                Arc::clone(&store),
                &[account()],
                local.clone(),
            )),
        );
        let mut cursor = None;
        let mut local_usage = 0_usize;
        let mut pages = 0_usize;
        let mut keys = BTreeSet::new();
        loop {
            let page = exporter
                .page(
                    VerifiedFederationBoundary::after_host_and_node_auth_checks(),
                    account(),
                    cursor,
                    MAX_PAGE_ROWS,
                )
                .await
                .expect("bounded page");
            pages += 1;
            for record in &page.records {
                assert!(keys.insert(record.key.clone()), "stable key repeated");
                if let FederatedPulseRow::Usage(snapshot) = &record.row {
                    assert_eq!(snapshot.machine, local, "mirrored row was re-exported");
                    local_usage += 1;
                }
            }
            let Some(next) = page.next_cursor else {
                break;
            };
            cursor = Some(next);
        }
        assert_eq!(local_usage, LOCAL_ROWS);
        assert!(pages > 20, "fixture must exercise resume across many pages");
        drop(store);
        remove_sqlite_test_files(&directory, &path);
    }

    #[tokio::test]
    async fn direct_exporter_attaches_origin_and_rejects_local_profile_metadata() {
        let local = machine("midnight");
        let local_profile = Profile {
            account_id: account(),
            name: ProfileName::new("claude").expect("profile"),
            vendor: Vendor::AnthropicOauth,
            config_dir: Some(std::path::PathBuf::from("/private/claude")),
            poll_interval_minutes: 15,
            monthly_budget_usd: None,
            api_key_env: None,
            api_key_file: None,
            refresh: crate::pulse::RefreshPolicy::InMemory,
            hidden: false,
            origin: ProfileOrigin::Local,
        };
        let exporter = DirectFederationExporter::new(
            local,
            Arc::new(FakeLocalSource {
                page: Mutex::new(Some(Ok(LocalFederationPage {
                    records: vec![
                        LocalFederationRecord::new(
                            FederationExportPosition::new(1, vec!["claude".to_owned()])
                                .expect("position"),
                            FederatedPulseRow::Profile(local_profile),
                        )
                        .expect("record"),
                    ],
                    next_cursor: None,
                }))),
            }),
        );
        let error = exporter
            .page(
                VerifiedFederationBoundary::after_host_and_node_auth_checks(),
                account(),
                None,
                10,
            )
            .await
            .expect_err("local profile rejected");
        assert_eq!(error.kind(), PulseErrorKind::Conflict);
    }
}
