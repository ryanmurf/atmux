//! Optional bounded Pulse push reporter.
//!
//! Tokens are resolved from external references, request bodies and bearer
//! values have redacted `Debug` implementations, and each deterministic chunk
//! keeps exactly the same request id and bytes across retries. The receiver can
//! therefore resume safely after a timeout without appending duplicate usage.

use std::{
    collections::BTreeSet,
    fmt,
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use http_body_util::{BodyExt as _, Full};
use hyper::{Method, Request, StatusCode, Uri, body::Bytes, header};
use hyper_util::rt::TokioIo;
use rand::{TryRngCore as _, rngs::OsRng};
use sha2::{Digest, Sha256};
use tokio::{net::TcpStream, sync::watch};

use super::{
    AccountId, Instant, MachineName, ProfileOrigin, PulseError, PulseErrorKind, PulseResult,
    collect::{HttpsJsonClient, SecretRef},
    ingest::{
        MAX_PUSH_BODY_BYTES, MAX_PUSH_ROWS, PUSH_VERSION, PushBatch, PushEnvelope,
        REPORTER_VERSION, ReportedProfile,
    },
    store::{
        ReporterCursorState, ReporterPendingChunk, ReporterPendingDraft, ReporterPendingPage,
        ReporterStreamKind, ReporterTokenPosition, Store,
    },
};

const MAX_REPORT_RESPONSE_BYTES: usize = 16 * 1024;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ENVELOPE_MARGIN_BYTES: usize = 1024;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30 * 60);
const COORDINATOR_PAGE_ROWS: usize = 500;
const MAX_COORDINATOR_PAGES_PER_KIND: usize = 32;

pub type ReporterFuture<T> = Pin<Box<dyn Future<Output = PulseResult<T>> + Send + 'static>>;

/// Secret-bearing request whose formatting exposes only endpoint and sizes.
pub struct ReporterRequest {
    endpoint: String,
    ingest_bearer: String,
    node_bearer: Option<String>,
    body: Vec<u8>,
}

impl ReporterRequest {
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Debug for ReporterRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReporterRequest")
            .field("endpoint", &self.endpoint)
            .field("ingest_bearer", &"[redacted]")
            .field(
                "node_bearer",
                &self.node_bearer.as_ref().map(|_| "[redacted]"),
            )
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Bounded response metadata; response bodies never cross this boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReporterResponse {
    pub status: u16,
    pub retry_after: Option<Duration>,
}

/// Injectable reporter transport.
pub trait ReporterTransport: Send + Sync + 'static {
    fn send(&self, request: ReporterRequest) -> ReporterFuture<ReporterResponse>;
}

/// Certificate-validating HTTPS client, plus plaintext HTTP restricted to an
/// actual loopback socket for deterministic local integration tests.
#[derive(Clone, Debug)]
pub struct HttpReporterTransport {
    https: HttpsJsonClient,
}

impl HttpReporterTransport {
    /// Builds a bounded reporter client using the operating-system root store.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when no usable trust roots exist.
    pub fn new() -> PulseResult<Self> {
        Ok(Self {
            https: HttpsJsonClient::new(MAX_REPORT_RESPONSE_BYTES)?,
        })
    }
}

impl ReporterTransport for HttpReporterTransport {
    fn send(&self, request: ReporterRequest) -> ReporterFuture<ReporterResponse> {
        let client = self.https.clone();
        Box::pin(async move {
            let endpoint = ReporterEndpoint::parse(&request.endpoint)?;
            let (authorization, ingest_header) = request_credentials(&endpoint, &request)?;
            if endpoint.https {
                let mut headers = vec![
                    (header::AUTHORIZATION.as_str(), authorization),
                    ("x-atmux-pulse-token", ingest_header),
                    ("x-atmux-pulse-version", PUSH_VERSION.to_string()),
                ];
                if endpoint.loopback && request.node_bearer.is_none() {
                    headers.retain(|(name, _)| *name != "x-atmux-pulse-token");
                }
                let response = client
                    .request(
                        Method::POST,
                        &request.endpoint,
                        &headers,
                        request.body,
                        Some("application/json"),
                    )
                    .await?;
                return Ok(ReporterResponse {
                    status: response.status.as_u16(),
                    retry_after: None,
                });
            }
            send_loopback_http(endpoint, request).await
        })
    }
}

#[derive(Clone, Debug)]
struct ReporterEndpoint {
    https: bool,
    loopback: bool,
    connect: Option<SocketAddr>,
    authority: String,
    target: String,
    identity: String,
}

impl ReporterEndpoint {
    fn parse(value: &str) -> PulseResult<Self> {
        let uri = value
            .parse::<Uri>()
            .map_err(|_| PulseError::configuration("Pulse report endpoint is invalid"))?;
        let scheme = uri
            .scheme_str()
            .ok_or_else(|| PulseError::configuration("Pulse report endpoint has no scheme"))?;
        let authority = uri
            .authority()
            .ok_or_else(|| PulseError::configuration("Pulse report endpoint has no authority"))?;
        if authority.as_str().contains('@')
            || uri
                .path_and_query()
                .and_then(hyper::http::uri::PathAndQuery::query)
                .is_some()
        {
            return Err(PulseError::configuration(
                "Pulse report endpoint cannot contain credentials or a query",
            ));
        }
        let host = uri
            .host()
            .ok_or_else(|| PulseError::configuration("Pulse report endpoint has no host"))?;
        let port = uri
            .port_u16()
            .unwrap_or(if scheme == "https" { 443 } else { 80 });
        let https = scheme == "https";
        let loopback = loopback_ip(host);
        let connect = if https {
            None
        } else if scheme == "http" {
            Some(loopback_socket(host, port).ok_or_else(|| {
                PulseError::configuration("plaintext Pulse reporting is allowed only to loopback")
            })?)
        } else {
            return Err(PulseError::configuration(
                "Pulse report endpoint must use HTTPS",
            ));
        };
        let target = uri
            .path_and_query()
            .map_or("/", hyper::http::uri::PathAndQuery::as_str)
            .to_owned();
        let normalized_host = normalize_endpoint_host(host);
        let identity_host = if normalized_host.parse::<Ipv6Addr>().is_ok() {
            format!("[{normalized_host}]")
        } else {
            normalized_host
        };
        let default_port = if https { 443 } else { 80 };
        let explicit_port = uri
            .port_u16()
            .filter(|configured| *configured != default_port)
            .map(|configured| format!(":{configured}"))
            .unwrap_or_default();
        let identity = format!("{scheme}://{identity_host}{explicit_port}{target}");
        Ok(Self {
            https,
            loopback,
            connect,
            authority: authority.as_str().to_owned(),
            target,
            identity,
        })
    }

    fn destination_key(&self) -> String {
        let digest = Sha256::digest(self.identity.as_bytes());
        let mut key = String::from("reporter-v1-");
        for byte in digest {
            use fmt::Write as _;
            write!(key, "{byte:02x}").expect("writing to a String cannot fail");
        }
        key
    }
}

fn normalize_endpoint_host(host: &str) -> String {
    let unbracketed = host.trim_matches(['[', ']']);
    if let Ok(address) = unbracketed.parse::<IpAddr>() {
        return address.to_string();
    }
    let lowercase = unbracketed.to_ascii_lowercase();
    lowercase.strip_suffix('.').unwrap_or(&lowercase).to_owned()
}

fn loopback_ip(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn loopback_socket(host: &str, port: u16) -> Option<SocketAddr> {
    if host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" {
        return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    }
    if host == "::1" || host == "[::1]" {
        return Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port));
    }
    host.parse::<IpAddr>()
        .ok()
        .filter(IpAddr::is_loopback)
        .map(|address| SocketAddr::new(address, port))
}

fn request_credentials(
    endpoint: &ReporterEndpoint,
    request: &ReporterRequest,
) -> PulseResult<(String, String)> {
    if let Some(node_bearer) = &request.node_bearer {
        return Ok((
            format!("Bearer {node_bearer}"),
            request.ingest_bearer.clone(),
        ));
    }
    if endpoint.loopback {
        return Ok((format!("Bearer {}", request.ingest_bearer), String::new()));
    }
    Err(PulseError::configuration(
        "non-loopback Pulse reporting requires a separate node token reference",
    ))
}

async fn send_loopback_http(
    endpoint: ReporterEndpoint,
    request: ReporterRequest,
) -> PulseResult<ReporterResponse> {
    let address = endpoint.connect.ok_or_else(|| {
        PulseError::new(PulseErrorKind::Internal, "missing loopback report target")
    })?;
    let stream = tokio::time::timeout(HTTP_CONNECT_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| PulseError::new(PulseErrorKind::Offline, "Pulse report connection timed out"))?
        .map_err(|_| PulseError::new(PulseErrorKind::Offline, "Pulse report connection failed"))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| {
            PulseError::new(
                PulseErrorKind::Offline,
                "Pulse report HTTP handshake failed",
            )
        })?;
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    let (authorization, ingest_header) = request_credentials(&endpoint, &request)?;
    let mut outgoing = Request::builder()
        .method(Method::POST)
        .uri(endpoint.target)
        .header(header::HOST, endpoint.authority)
        .header(header::AUTHORIZATION, authorization)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-atmux-pulse-version", PUSH_VERSION);
    if !ingest_header.is_empty() {
        outgoing = outgoing.header("x-atmux-pulse-token", ingest_header);
    }
    let outgoing = outgoing
        .body(Full::new(Bytes::from(request.body)))
        .map_err(|_| PulseError::configuration("Pulse report request is invalid"))?;
    let response = tokio::time::timeout(HTTP_REQUEST_TIMEOUT, sender.send_request(outgoing))
        .await
        .map_err(|_| PulseError::new(PulseErrorKind::Offline, "Pulse report request timed out"))?
        .map_err(|_| PulseError::new(PulseErrorKind::Offline, "Pulse report request failed"))?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .map(|duration| duration.min(MAX_RETRY_AFTER));
    collect_response_bounded(response.into_body()).await?;
    driver.abort();
    Ok(ReporterResponse {
        status,
        retry_after,
    })
}

async fn collect_response_bounded(mut body: hyper::body::Incoming) -> PulseResult<()> {
    let mut bytes = 0_usize;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| {
            PulseError::new(
                PulseErrorKind::Upstream,
                "Pulse report response was unreadable",
            )
        })?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        bytes = bytes.saturating_add(data.len());
        if bytes > MAX_REPORT_RESPONSE_BYTES {
            return Err(PulseError::new(
                PulseErrorKind::Upstream,
                "Pulse report response exceeded its size bound",
            ));
        }
    }
    Ok(())
}

/// Bounded exponential backoff with symmetric random jitter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReporterBackoff {
    pub base: Duration,
    pub maximum: Duration,
    pub max_attempts: u8,
    pub jitter_percent: u8,
}

impl Default for ReporterBackoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            maximum: Duration::from_secs(5 * 60),
            max_attempts: 8,
            jitter_percent: 20,
        }
    }
}

impl ReporterBackoff {
    fn validate(self) -> PulseResult<Self> {
        if self.base.is_zero()
            || self.maximum < self.base
            || self.maximum > MAX_RETRY_AFTER
            || self.max_attempts == 0
            || self.max_attempts > 16
            || self.jitter_percent > 50
        {
            return Err(PulseError::configuration(
                "Pulse reporter backoff bounds are invalid",
            ));
        }
        Ok(self)
    }

    fn delay(self, attempt: u8, entropy: u64) -> Duration {
        let shift = u32::from(attempt.saturating_sub(1).min(20));
        let multiplier = 1_u32.checked_shl(shift).unwrap_or(u32::MAX);
        let nominal = self.base.saturating_mul(multiplier).min(self.maximum);
        if self.jitter_percent == 0 {
            return nominal;
        }
        let range = u128::from(self.jitter_percent) * 2 + 1;
        let selected = u128::from(entropy) % range;
        let signed = i128::try_from(selected).unwrap_or(0) - i128::from(self.jitter_percent);
        let nanos = nominal.as_nanos();
        let adjustment = nanos
            .saturating_mul(signed.unsigned_abs())
            .checked_div(100)
            .unwrap_or(0);
        let jittered = if signed.is_negative() {
            nanos.saturating_sub(adjustment)
        } else {
            nanos.saturating_add(adjustment)
        };
        Duration::from_nanos(u64::try_from(jittered).unwrap_or(u64::MAX))
    }
}

/// Result of a bounded reporting cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReporterOutcome {
    pub chunks_sent: usize,
    pub rows_sent: usize,
    pub cancelled: bool,
}

/// Client used by collector-completion and scheduled resync hooks.
pub struct PulseReporter {
    endpoint: String,
    destination_key: String,
    token: SecretRef,
    node_token: Option<SecretRef>,
    transport: Arc<dyn ReporterTransport>,
    backoff: ReporterBackoff,
}

/// Per-account completion result. One offline account never prevents another
/// account from reporting.
pub struct AccountReporterOutcome {
    pub account_id: AccountId,
    pub result: PulseResult<ReporterOutcome>,
}

/// Store-backed completion coordinator for `PersistingJobRunner`.
///
/// It reads only local profile metadata and rows belonging to the configured
/// local machine. Usage and token pages use durable per-destination SQL keyset
/// cursors; progress advances only after the complete remote page succeeds.
/// Replaying after a crash therefore produces identical request bytes and ids.
pub struct StoreReporterCoordinator {
    store: Arc<dyn Store>,
    accounts: Arc<[AccountId]>,
    machine: MachineName,
    reporter: Arc<PulseReporter>,
    destination_key: String,
}

impl StoreReporterCoordinator {
    #[must_use]
    pub fn new(
        store: Arc<dyn Store>,
        accounts: Arc<[AccountId]>,
        machine: MachineName,
        reporter: Arc<PulseReporter>,
    ) -> Self {
        let destination_key = reporter.destination_key.clone();
        Self {
            store,
            accounts,
            machine,
            reporter,
            destination_key,
        }
    }

    /// Builds and reports one bounded account batch per configured account.
    ///
    /// The returned vector contains one entry per attempted account. A
    /// transport or storage failure in one entry does not cancel later entries.
    pub async fn report_completed(
        &self,
        _completed_at: Instant,
        cancellation: &mut watch::Receiver<bool>,
    ) -> Vec<AccountReporterOutcome> {
        let mut outcomes = Vec::with_capacity(self.accounts.len());
        for account_id in self.accounts.iter().copied() {
            if *cancellation.borrow() {
                outcomes.push(AccountReporterOutcome {
                    account_id,
                    result: Ok(ReporterOutcome {
                        cancelled: true,
                        ..ReporterOutcome::default()
                    }),
                });
                break;
            }
            let result = self.report_account(account_id, cancellation).await;
            outcomes.push(AccountReporterOutcome { account_id, result });
        }
        outcomes
    }

    async fn report_account(
        &self,
        account_id: AccountId,
        cancellation: &mut watch::Receiver<bool>,
    ) -> PulseResult<ReporterOutcome> {
        let metadata = self.assemble_metadata(account_id).await?;
        let mut outcome = self
            .reporter
            .report_batch(account_id, self.machine.clone(), metadata, cancellation)
            .await?;
        if outcome.cancelled {
            return Ok(outcome);
        }
        let mut state = self
            .store
            .load_reporter_cursor(
                account_id,
                self.machine.clone(),
                self.destination_key.clone(),
            )
            .await?;
        self.report_usage_pages(account_id, &mut state, cancellation, &mut outcome)
            .await?;
        if !outcome.cancelled {
            self.report_token_pages(account_id, &mut state, cancellation, &mut outcome)
                .await?;
        }
        Ok(outcome)
    }

    async fn assemble_metadata(&self, account_id: AccountId) -> PulseResult<PushBatch> {
        let profiles = self
            .store
            .list_profiles(account_id)
            .await?
            .into_iter()
            .filter(|profile| profile.origin == ProfileOrigin::Local)
            .collect::<Vec<_>>();
        let profile_names = profiles
            .iter()
            .map(|profile| profile.name.clone())
            .collect::<BTreeSet<_>>();
        let contexts = self
            .store
            .list_context_sessions(account_id, None)
            .await?
            .into_iter()
            .filter(|row| row.machine == self.machine && profile_names.contains(&row.profile))
            .collect();
        let gemini_quotas = self.store.list_gemini_quotas(account_id).await?;
        let profiles = profiles
            .into_iter()
            .map(|profile| ReportedProfile {
                name: profile.name,
                vendor: profile.vendor,
                poll_interval_minutes: profile.poll_interval_minutes,
                monthly_budget_usd: profile.monthly_budget_usd,
            })
            .collect();
        Ok(PushBatch {
            profiles,
            snapshots: Vec::new(),
            token_grains: Vec::new(),
            context_sessions: contexts,
            gemini_quotas,
        })
    }

    async fn report_usage_pages(
        &self,
        account_id: AccountId,
        state: &mut ReporterCursorState,
        cancellation: &mut watch::Receiver<bool>,
        outcome: &mut ReporterOutcome,
    ) -> PulseResult<()> {
        for _ in 0..MAX_COORDINATOR_PAGES_PER_KIND {
            let pending = if let Some(pending) = self
                .store
                .load_reporter_pending(
                    account_id,
                    self.machine.clone(),
                    self.destination_key.clone(),
                    ReporterStreamKind::Usage,
                )
                .await?
            {
                pending
            } else {
                let rows = self
                    .store
                    .local_reporter_usage_page(
                        account_id,
                        self.machine.clone(),
                        state.usage_after_id,
                        COORDINATOR_PAGE_ROWS,
                    )
                    .await?;
                let Some(last_id) = rows.last().map(|row| row.id) else {
                    break;
                };
                let page = PushBatch {
                    snapshots: rows.into_iter().map(|row| row.snapshot).collect(),
                    ..PushBatch::default()
                };
                let mut next = state.clone();
                next.usage_after_id = last_id;
                let draft = reporter_pending_draft(
                    account_id,
                    &self.machine,
                    ReporterStreamKind::Usage,
                    state.clone(),
                    next,
                    page,
                )?;
                self.store
                    .prepare_reporter_pending(
                        account_id,
                        self.machine.clone(),
                        self.destination_key.clone(),
                        draft,
                    )
                    .await?
            };
            ensure_pending_expected(&pending, state)?;
            let sent = self
                .reporter
                .report_pending_page(&pending, cancellation)
                .await?;
            add_outcome(outcome, sent);
            if sent.cancelled {
                break;
            }
            *state = self
                .store
                .commit_reporter_pending(
                    account_id,
                    self.machine.clone(),
                    self.destination_key.clone(),
                    ReporterStreamKind::Usage,
                    pending.id,
                )
                .await?;
        }
        Ok(())
    }

    async fn report_token_pages(
        &self,
        account_id: AccountId,
        state: &mut ReporterCursorState,
        cancellation: &mut watch::Receiver<bool>,
        outcome: &mut ReporterOutcome,
    ) -> PulseResult<()> {
        for _ in 0..MAX_COORDINATOR_PAGES_PER_KIND {
            let pending = if let Some(pending) = self
                .store
                .load_reporter_pending(
                    account_id,
                    self.machine.clone(),
                    self.destination_key.clone(),
                    ReporterStreamKind::Token,
                )
                .await?
            {
                pending
            } else {
                let rows = self
                    .store
                    .local_reporter_token_page(
                        account_id,
                        self.machine.clone(),
                        state.token_after.clone(),
                        COORDINATOR_PAGE_ROWS,
                    )
                    .await?;
                let Some(last) = rows.last() else {
                    if state.token_after.is_some() {
                        let mut next = state.clone();
                        next.token_after = None;
                        next.token_generation = next.token_generation.saturating_add(1);
                        *state = self
                            .store
                            .advance_reporter_cursor(
                                account_id,
                                self.machine.clone(),
                                self.destination_key.clone(),
                                state.clone(),
                                next,
                            )
                            .await?;
                    }
                    break;
                };
                let next_position = ReporterTokenPosition::from_grain(last)?;
                let page = PushBatch {
                    token_grains: rows,
                    ..PushBatch::default()
                };
                let mut next = state.clone();
                next.token_after = Some(next_position);
                let draft = reporter_pending_draft(
                    account_id,
                    &self.machine,
                    ReporterStreamKind::Token,
                    state.clone(),
                    next,
                    page,
                )?;
                self.store
                    .prepare_reporter_pending(
                        account_id,
                        self.machine.clone(),
                        self.destination_key.clone(),
                        draft,
                    )
                    .await?
            };
            ensure_pending_expected(&pending, state)?;
            let sent = self
                .reporter
                .report_pending_page(&pending, cancellation)
                .await?;
            add_outcome(outcome, sent);
            if sent.cancelled {
                break;
            }
            *state = self
                .store
                .commit_reporter_pending(
                    account_id,
                    self.machine.clone(),
                    self.destination_key.clone(),
                    ReporterStreamKind::Token,
                    pending.id,
                )
                .await?;
        }
        Ok(())
    }
}

fn ensure_pending_expected(
    pending: &ReporterPendingPage,
    state: &ReporterCursorState,
) -> PulseResult<()> {
    if pending.draft.expected != *state {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "Pulse reporter outbox does not match its durable cursor",
        ));
    }
    Ok(())
}

fn reporter_pending_draft(
    account_id: AccountId,
    machine: &MachineName,
    kind: ReporterStreamKind,
    expected: ReporterCursorState,
    next: ReporterCursorState,
    batch: PushBatch,
) -> PulseResult<ReporterPendingDraft> {
    let chunks = deterministic_page_chunks(account_id, machine, batch)?;
    let mut pending = Vec::with_capacity(chunks.len());
    for (index, (body, rows)) in chunks.into_iter().enumerate() {
        let mut envelope = serde_json::from_slice::<PushEnvelope>(&body).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Internal,
                "failed to decode a generated Pulse reporter document",
            )
        })?;
        envelope.request_id = pending_request_id(
            account_id,
            machine,
            kind,
            &expected,
            &next,
            index,
            &envelope.batch,
        )?;
        let request_id = envelope.request_id.clone();
        let body = envelope.encode()?;
        pending.push(ReporterPendingChunk {
            request_id,
            body,
            rows,
        });
    }
    let draft = ReporterPendingDraft {
        kind,
        expected,
        next,
        chunks: pending,
    };
    draft.validate(account_id, machine)?;
    Ok(draft)
}

fn pending_request_id(
    account_id: AccountId,
    machine: &MachineName,
    kind: ReporterStreamKind,
    expected: &ReporterCursorState,
    next: &ReporterCursorState,
    index: usize,
    batch: &PushBatch,
) -> PulseResult<String> {
    let canonical = serde_json::to_vec(&(
        "atmux-pulse-reporter-pending-v1",
        PUSH_VERSION,
        REPORTER_VERSION,
        account_id,
        machine,
        kind,
        expected,
        next,
        index,
        batch,
    ))
    .map_err(|_| {
        PulseError::new(
            PulseErrorKind::Internal,
            "failed to encode a Pulse reporter outbox identity",
        )
    })?;
    Ok(content_request_id(&canonical))
}

fn add_outcome(total: &mut ReporterOutcome, page: ReporterOutcome) {
    total.chunks_sent = total.chunks_sent.saturating_add(page.chunks_sent);
    total.rows_sent = total.rows_sent.saturating_add(page.rows_sent);
    total.cancelled |= page.cancelled;
}

impl fmt::Debug for PulseReporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PulseReporter")
            .field("endpoint", &self.endpoint)
            .field("destination_key", &self.destination_key)
            .field("token", &self.token)
            .field("node_token", &self.node_token)
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

impl PulseReporter {
    /// Builds a reporter without resolving its external credential yet.
    ///
    /// # Errors
    ///
    /// Rejects unsafe endpoints and invalid retry bounds.
    pub fn new(
        endpoint: String,
        token: SecretRef,
        transport: Arc<dyn ReporterTransport>,
        backoff: ReporterBackoff,
    ) -> PulseResult<Self> {
        let parsed = ReporterEndpoint::parse(&endpoint)?;
        if !parsed.loopback {
            return Err(PulseError::configuration(
                "non-loopback Pulse reporting requires a separate node token reference",
            ));
        }
        Ok(Self {
            endpoint,
            destination_key: parsed.destination_key(),
            token,
            node_token: None,
            transport,
            backoff: backoff.validate()?,
        })
    }

    /// Builds a reporter with separate outer node/proxy and ingest credentials.
    ///
    /// Both credentials remain external references and are resolved only when
    /// a completion push actually runs.
    ///
    /// # Errors
    ///
    /// Rejects unsafe endpoints and invalid retry bounds.
    pub fn new_with_node_token(
        endpoint: String,
        token: SecretRef,
        node_token: SecretRef,
        transport: Arc<dyn ReporterTransport>,
        backoff: ReporterBackoff,
    ) -> PulseResult<Self> {
        let parsed = ReporterEndpoint::parse(&endpoint)?;
        Ok(Self {
            endpoint,
            destination_key: parsed.destination_key(),
            token,
            node_token: Some(node_token),
            transport,
            backoff: backoff.validate()?,
        })
    }

    /// Deterministically chunks and reports one machine's completed data.
    ///
    /// Every retry uses identical bytes. A new process rebuilding the same
    /// batch derives the same request id, so receiver replay state remains
    /// resumable across reporter restarts.
    ///
    /// # Errors
    ///
    /// Returns a configuration/authentication error for an unavailable token,
    /// invalid endpoint, or rejected credentials; invalid input for an
    /// oversized batch; or a retryable transport error after its bounded retry
    /// budget is exhausted.
    pub async fn report_batch(
        &self,
        account_id: AccountId,
        machine: MachineName,
        batch: PushBatch,
        cancellation: &mut watch::Receiver<bool>,
    ) -> PulseResult<ReporterOutcome> {
        let chunks = deterministic_chunks(account_id, &machine, batch)?;
        self.report_chunks(chunks, cancellation).await
    }

    async fn report_pending_page(
        &self,
        pending: &ReporterPendingPage,
        cancellation: &mut watch::Receiver<bool>,
    ) -> PulseResult<ReporterOutcome> {
        let chunks = pending
            .draft
            .chunks
            .iter()
            .map(|chunk| (chunk.body.clone(), chunk.rows))
            .collect();
        self.report_chunks(chunks, cancellation).await
    }

    async fn report_chunks(
        &self,
        chunks: Vec<(Vec<u8>, usize)>,
        cancellation: &mut watch::Receiver<bool>,
    ) -> PulseResult<ReporterOutcome> {
        let resolved = self.token.resolve()?;
        let resolved_node = self
            .node_token
            .as_ref()
            .map(SecretRef::resolve)
            .transpose()?;
        if resolved_node.as_ref().is_some_and(|node| {
            constant_time_eq(node.expose().as_bytes(), resolved.expose().as_bytes())
        }) {
            return Err(PulseError::configuration(
                "Pulse ingest and node credentials must be distinct",
            ));
        }
        let mut outcome = ReporterOutcome::default();
        for (body, rows) in chunks {
            if *cancellation.borrow() {
                outcome.cancelled = true;
                return Ok(outcome);
            }
            let mut attempt = 0_u8;
            loop {
                attempt = attempt.saturating_add(1);
                let request = ReporterRequest {
                    endpoint: self.endpoint.clone(),
                    ingest_bearer: resolved.expose().to_owned(),
                    node_bearer: resolved_node
                        .as_ref()
                        .map(|secret| secret.expose().to_owned()),
                    body: body.clone(),
                };
                match self.transport.send(request).await {
                    Ok(response) if (200..300).contains(&response.status) => break,
                    Ok(response)
                        if response.status == StatusCode::UNAUTHORIZED.as_u16()
                            || response.status == StatusCode::FORBIDDEN.as_u16() =>
                    {
                        return Err(PulseError::new(
                            PulseErrorKind::Authentication,
                            "Pulse reporter authentication was rejected",
                        ));
                    }
                    Ok(response)
                        if response.status == StatusCode::TOO_MANY_REQUESTS.as_u16()
                            || response.status == StatusCode::REQUEST_TIMEOUT.as_u16()
                            || response.status >= 500 =>
                    {
                        if attempt >= self.backoff.max_attempts {
                            return Err(PulseError::new(
                                PulseErrorKind::RateLimited,
                                "Pulse reporter retry budget was exhausted",
                            ));
                        }
                        let delay = response
                            .retry_after
                            .unwrap_or_else(|| self.backoff.delay(attempt, random_u64()))
                            .min(MAX_RETRY_AFTER);
                        if wait_or_cancel(delay, cancellation).await {
                            outcome.cancelled = true;
                            return Ok(outcome);
                        }
                    }
                    Ok(_) => {
                        return Err(PulseError::new(
                            PulseErrorKind::Upstream,
                            "Pulse receiver rejected the reporter document",
                        ));
                    }
                    Err(error) if error.kind().is_retryable() => {
                        if attempt >= self.backoff.max_attempts {
                            return Err(PulseError::new(
                                error.kind(),
                                "Pulse reporter retry budget was exhausted",
                            ));
                        }
                        let delay = self.backoff.delay(attempt, random_u64());
                        if wait_or_cancel(delay, cancellation).await {
                            outcome.cancelled = true;
                            return Ok(outcome);
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            outcome.chunks_sent = outcome.chunks_sent.saturating_add(1);
            outcome.rows_sent = outcome.rows_sent.saturating_add(rows);
        }
        Ok(outcome)
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

async fn wait_or_cancel(delay: Duration, cancellation: &mut watch::Receiver<bool>) -> bool {
    if *cancellation.borrow() {
        return true;
    }
    tokio::select! {
        () = tokio::time::sleep(delay) => false,
        changed = cancellation.changed() => changed.is_err() || *cancellation.borrow(),
    }
}

#[derive(Clone)]
enum PushRow {
    Profile(super::ingest::ReportedProfile),
    Snapshot(super::UsageSnapshot),
    Token(super::TokenGrain),
    Context(super::ContextSession),
    Gemini(super::GeminiQuota),
}

fn deterministic_chunks(
    account_id: AccountId,
    machine: &MachineName,
    batch: PushBatch,
) -> PulseResult<Vec<(Vec<u8>, usize)>> {
    let mut chunks = Vec::new();
    append_row_chunks(
        &mut chunks,
        batch.profiles.into_iter().map(PushRow::Profile).collect(),
        account_id,
        machine,
    )?;
    // Usage snapshots are append-only and have no receiver-side natural key.
    // Isolating each one gives it a stable replay id even if a reporter loses
    // its in-memory high-water mark and later discovers newer snapshots.
    for snapshot in batch.snapshots {
        append_row_chunks(
            &mut chunks,
            vec![PushRow::Snapshot(snapshot)],
            account_id,
            machine,
        )?;
    }
    let mut upserts = Vec::with_capacity(
        batch
            .token_grains
            .len()
            .saturating_add(batch.context_sessions.len())
            .saturating_add(batch.gemini_quotas.len()),
    );
    upserts.extend(batch.token_grains.into_iter().map(PushRow::Token));
    upserts.extend(batch.context_sessions.into_iter().map(PushRow::Context));
    upserts.extend(batch.gemini_quotas.into_iter().map(PushRow::Gemini));
    append_row_chunks(&mut chunks, upserts, account_id, machine)?;
    Ok(chunks)
}

/// Chunks a durable SQL page as one stable ordered row stream. The coordinator
/// does not advance its Store cursor until every returned chunk succeeds, so a
/// crash rebuilds the same page and therefore the same receiver replay ids.
fn deterministic_page_chunks(
    account_id: AccountId,
    machine: &MachineName,
    batch: PushBatch,
) -> PulseResult<Vec<(Vec<u8>, usize)>> {
    let mut rows = Vec::with_capacity(batch.row_count());
    rows.extend(batch.profiles.into_iter().map(PushRow::Profile));
    rows.extend(batch.snapshots.into_iter().map(PushRow::Snapshot));
    rows.extend(batch.token_grains.into_iter().map(PushRow::Token));
    rows.extend(batch.context_sessions.into_iter().map(PushRow::Context));
    rows.extend(batch.gemini_quotas.into_iter().map(PushRow::Gemini));
    let mut chunks = Vec::new();
    append_row_chunks(&mut chunks, rows, account_id, machine)?;
    Ok(chunks)
}

fn append_row_chunks(
    chunks: &mut Vec<(Vec<u8>, usize)>,
    mut rows: Vec<PushRow>,
    account_id: AccountId,
    machine: &MachineName,
) -> PulseResult<()> {
    while !rows.is_empty() {
        let mut take = rows.len().min(MAX_PUSH_ROWS);
        loop {
            let batch = batch_from_rows(&rows[..take]);
            let canonical = serde_json::to_vec(&batch).map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Internal,
                    "failed to encode Pulse reporter batch",
                )
            })?;
            if canonical.len() > MAX_PUSH_BODY_BYTES.saturating_sub(ENVELOPE_MARGIN_BYTES) {
                if take == 1 {
                    return Err(PulseError::invalid_input(
                        "one Pulse reporter row exceeds the request bound",
                    ));
                }
                take = take.div_ceil(2);
                continue;
            }
            let request_id = content_request_id(&canonical);
            let body = PushEnvelope {
                version: PUSH_VERSION,
                request_id,
                reporter_version: REPORTER_VERSION.to_owned(),
                account_id: Some(account_id),
                machine: Some(machine.clone()),
                batch,
            }
            .encode()?;
            chunks.push((body, take));
            rows.drain(..take);
            break;
        }
    }
    Ok(())
}

fn batch_from_rows(rows: &[PushRow]) -> PushBatch {
    let mut batch = PushBatch::default();
    for row in rows {
        match row {
            PushRow::Profile(value) => batch.profiles.push(value.clone()),
            PushRow::Snapshot(value) => batch.snapshots.push(value.clone()),
            PushRow::Token(value) => batch.token_grains.push(value.clone()),
            PushRow::Context(value) => batch.context_sessions.push(value.clone()),
            PushRow::Gemini(value) => batch.gemini_quotas.push(value.clone()),
        }
    }
    batch
}

fn content_request_id(canonical: &[u8]) -> String {
    let digest = Sha256::digest(canonical);
    let mut text = String::from("push-");
    for byte in &digest[..16] {
        use fmt::Write as _;
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
    }
    text
}

fn random_u64() -> u64 {
    let mut bytes = [0_u8; 8];
    if OsRng.try_fill_bytes(&mut bytes).is_err() {
        return 0;
    }
    u64::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pulse::{
        CollectionOutcome, Instant, Percent, ProfileName, QuotaWindow, QuotaWindowKind,
        UsageSnapshot, Vendor, ingest::ReportedProfile,
    };

    fn account() -> AccountId {
        AccountId::new(1).expect("account")
    }

    fn machine() -> MachineName {
        MachineName::new("midnight").expect("machine")
    }

    fn one_profile() -> PushBatch {
        PushBatch {
            profiles: vec![ReportedProfile {
                name: ProfileName::new("claude-max").expect("profile"),
                vendor: Vendor::AnthropicOauth,
                poll_interval_minutes: 15,
                monthly_budget_usd: None,
            }],
            ..PushBatch::default()
        }
    }

    fn usage(polled_at: i64) -> UsageSnapshot {
        UsageSnapshot {
            account_id: account(),
            profile: ProfileName::new("claude-max").expect("profile"),
            machine: machine(),
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

    #[test]
    fn endpoint_allows_only_https_or_literal_loopback() {
        assert!(ReporterEndpoint::parse("https://pulse.example.test/ingest").is_ok());
        assert!(ReporterEndpoint::parse("http://127.0.0.1:7345/ingest").is_ok());
        assert!(ReporterEndpoint::parse("http://10.0.0.4/ingest").is_err());
        assert!(ReporterEndpoint::parse("https://user@example.test/ingest").is_err());
        assert!(ReporterEndpoint::parse("https://example.test/ingest?q=1").is_err());
    }

    #[test]
    fn destination_key_normalizes_host_and_default_port_without_storing_url() {
        let first = ReporterEndpoint::parse("http://LOCALHOST:80/ingest").expect("endpoint");
        let second = ReporterEndpoint::parse("http://localhost/ingest").expect("endpoint");
        assert_eq!(first.destination_key(), second.destination_key());
        assert!(first.destination_key().starts_with("reporter-v1-"));
        assert!(!first.destination_key().contains("localhost"));
        let other = ReporterEndpoint::parse("http://localhost/other").expect("endpoint");
        assert_ne!(first.destination_key(), other.destination_key());

        let rooted_dns = ReporterEndpoint::parse("https://EXAMPLE.test.:443/ingest")
            .expect("rooted DNS endpoint");
        let plain_dns =
            ReporterEndpoint::parse("https://example.test/ingest").expect("DNS endpoint");
        assert_eq!(rooted_dns.destination_key(), plain_dns.destination_key());

        let expanded_ipv6 = ReporterEndpoint::parse("https://[0:0:0:0:0:0:0:1]:443/ingest")
            .expect("expanded IPv6 endpoint");
        let compressed_ipv6 =
            ReporterEndpoint::parse("https://[::1]/ingest").expect("compressed IPv6 endpoint");
        assert_eq!(
            expanded_ipv6.destination_key(),
            compressed_ipv6.destination_key()
        );
    }

    #[test]
    fn deterministic_chunks_have_stable_replay_ids_and_bounds() {
        let first = deterministic_chunks(account(), &machine(), one_profile()).expect("chunks");
        let second = deterministic_chunks(account(), &machine(), one_profile()).expect("chunks");
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        assert!(first[0].0.len() <= MAX_PUSH_BODY_BYTES);
        let envelope: PushEnvelope = serde_json::from_slice(&first[0].0).expect("envelope");
        assert!(envelope.request_id.starts_with("push-"));
        assert_eq!(envelope.batch.row_count(), 1);
    }

    #[test]
    fn append_only_usage_keeps_prior_snapshot_replay_bytes_stable() {
        let mut first_batch = one_profile();
        first_batch.snapshots.push(usage(1_000_000));
        let first = deterministic_chunks(account(), &machine(), first_batch).expect("chunks");
        let mut second_batch = one_profile();
        second_batch.snapshots.push(usage(1_000_000));
        second_batch.snapshots.push(usage(2_000_000));
        let second = deterministic_chunks(account(), &machine(), second_batch).expect("chunks");
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 3);
        assert_eq!(first[0], second[0]);
        assert_eq!(first[1], second[1]);
    }

    #[test]
    fn durable_usage_page_is_grouped_and_rebuilds_identical_bytes() {
        let batch = PushBatch {
            snapshots: (0..COORDINATOR_PAGE_ROWS)
                .map(|index| usage(1_000_000 + i64::try_from(index).expect("index")))
                .collect(),
            ..PushBatch::default()
        };
        let first =
            deterministic_page_chunks(account(), &machine(), batch.clone()).expect("first page");
        let second = deterministic_page_chunks(account(), &machine(), batch).expect("second page");
        assert_eq!(first, second);
        assert!(first.len() < COORDINATOR_PAGE_ROWS);
        assert_eq!(
            first.iter().map(|(_, rows)| rows).sum::<usize>(),
            COORDINATOR_PAGE_ROWS
        );
        assert!(
            first
                .iter()
                .all(|(body, _)| body.len() <= MAX_PUSH_BODY_BYTES)
        );
    }

    #[test]
    fn outbox_ids_distinguish_identical_content_at_distinct_cursor_positions() {
        let batch = PushBatch {
            snapshots: vec![usage(1_000_000)],
            ..PushBatch::default()
        };
        let first_expected = ReporterCursorState::default();
        let mut first_next = first_expected.clone();
        first_next.usage_after_id = 10;
        let first = reporter_pending_draft(
            account(),
            &machine(),
            ReporterStreamKind::Usage,
            first_expected.clone(),
            first_next.clone(),
            batch.clone(),
        )
        .expect("first outbox page");
        let replay = reporter_pending_draft(
            account(),
            &machine(),
            ReporterStreamKind::Usage,
            first_expected,
            first_next.clone(),
            batch.clone(),
        )
        .expect("replayed outbox page");
        assert_eq!(first, replay);

        let mut second_next = first_next.clone();
        second_next.usage_after_id = 20;
        let second = reporter_pending_draft(
            account(),
            &machine(),
            ReporterStreamKind::Usage,
            first_next,
            second_next,
            batch,
        )
        .expect("second outbox page");
        assert_ne!(first.chunks[0].request_id, second.chunks[0].request_id);
        assert_ne!(first.chunks[0].body, second.chunks[0].body);
    }

    #[test]
    fn request_debug_never_contains_token_or_body() {
        let request = ReporterRequest {
            endpoint: "https://example.test/ingest".to_owned(),
            ingest_bearer: "top-secret-ingest-token".to_owned(),
            node_bearer: Some("top-secret-node-token".to_owned()),
            body: b"private body".to_vec(),
        };
        let debug = format!("{request:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("top-secret-ingest-token"));
        assert!(!debug.contains("top-secret-node-token"));
        assert!(!debug.contains("private body"));
    }

    #[test]
    fn remote_credentials_are_split_and_never_fall_back() {
        let remote = ReporterEndpoint::parse("https://example.test/ingest").expect("endpoint");
        let split = ReporterRequest {
            endpoint: "https://example.test/ingest".to_owned(),
            ingest_bearer: "ingest-only".to_owned(),
            node_bearer: Some("node-only".to_owned()),
            body: Vec::new(),
        };
        assert_eq!(
            request_credentials(&remote, &split).expect("split credentials"),
            ("Bearer node-only".to_owned(), "ingest-only".to_owned())
        );
        let ingest_only = ReporterRequest {
            endpoint: split.endpoint.clone(),
            ingest_bearer: split.ingest_bearer.clone(),
            node_bearer: None,
            body: Vec::new(),
        };
        assert!(request_credentials(&remote, &ingest_only).is_err());

        let loopback = ReporterEndpoint::parse("http://127.0.0.1:7345/ingest").expect("endpoint");
        assert_eq!(
            request_credentials(&loopback, &ingest_only).expect("loopback fallback"),
            ("Bearer ingest-only".to_owned(), String::new())
        );
    }

    #[test]
    fn backoff_is_exponential_bounded_and_jittered() {
        let policy = ReporterBackoff {
            base: Duration::from_secs(10),
            maximum: Duration::from_secs(60),
            max_attempts: 8,
            jitter_percent: 20,
        };
        assert_eq!(policy.delay(1, 20), Duration::from_secs(10));
        assert_eq!(policy.delay(2, 20), Duration::from_secs(20));
        assert!(policy.delay(8, 40) <= Duration::from_secs(72));
    }

    #[tokio::test]
    async fn cancellation_interrupts_retry_without_sending_new_bytes() {
        let (sender, mut cancellation) = watch::channel(false);
        let task = tokio::spawn(async move {
            let _ = sender.send(true);
        });
        task.await.expect("cancel task");
        assert!(wait_or_cancel(Duration::from_secs(30), &mut cancellation).await);
    }

    #[test]
    fn plaintext_token_reference_is_not_part_of_serializable_reporter_state() {
        let reference = SecretRef::Environment {
            name: "ATMUX_PULSE_REPORT_TOKEN".to_owned(),
        };
        let debug = format!("{reference:?}");
        assert!(debug.contains("ATMUX_PULSE_REPORT_TOKEN"));
        assert!(!debug.contains("Bearer"));
    }
}
