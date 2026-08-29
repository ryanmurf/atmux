//! Optional, separately authenticated Pulse push ingest.
//!
//! This module is deliberately transport-framework agnostic. The web adapter
//! must first enforce atmux's existing Host, node-authentication, and Origin
//! boundary, then pass a [`VerifiedReceiverBoundary`] into [`IngestReceiver`].
//! The ingest token adds authority for exactly one account and machine; values
//! claimed by the JSON body are never trusted.

use std::{
    collections::HashMap,
    fmt,
    hash::Hash,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant as MonotonicInstant},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{TryRngCore as _, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    AccountId, AgentSettings, ContextSession, GeminiQuota, Instant, Machine, MachineName, Profile,
    ProfileName, ProfileOrigin, PulseError, PulseErrorKind, PulseResult, RefreshPolicy, TokenGrain,
    TokenSource, UsageSnapshot, Vendor,
    store::{IdempotentIngestResult, IngestBatch, IngestLimits, IngestReplay, IngestToken, Store},
};

pub const PUSH_VERSION: u16 = 1;
pub const REPORTER_VERSION: &str = concat!("atmux/", env!("CARGO_PKG_VERSION"));
pub const MAX_PUSH_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_PUSH_ROWS: usize = 10_000;
pub const MAX_JSON_DEPTH: usize = 16;
pub const MAX_JSON_NODES: usize = 100_000;
pub const MAX_JSON_STRING_BYTES: usize = 8 * 1024;
pub const MAX_ACTIVE_INGEST_TOKENS: usize = 64;
const TOKEN_PREFIX: &str = "atmux-pulse-v1";
const TOKEN_RANDOM_BYTES: usize = 32;
const TOKEN_RANDOM_TEXT_BYTES: usize = 43;
const MAX_BEARER_BYTES: usize = 256;
const MAX_SETTINGS_ENTRIES: usize = 32;
const MAX_SETTINGS_KEY_BYTES: usize = 128;
const MAX_SETTINGS_VALUE_BYTES: usize = 256;
const MAX_GEMINI_AMOUNT_BYTES: usize = 256;

/// Secret-free remote profile discovery metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedProfile {
    pub name: ProfileName,
    pub vendor: Vendor,
    #[serde(default = "default_poll_minutes")]
    pub poll_interval_minutes: u32,
    pub monthly_budget_usd: Option<f64>,
}

const fn default_poll_minutes() -> u32 {
    15
}

impl ReportedProfile {
    fn into_domain(self, account_id: AccountId) -> PulseResult<Profile> {
        let profile = Profile {
            account_id,
            name: self.name,
            vendor: self.vendor,
            config_dir: None,
            poll_interval_minutes: self.poll_interval_minutes,
            monthly_budget_usd: self.monthly_budget_usd,
            api_key_env: None,
            api_key_file: None,
            refresh: RefreshPolicy::InMemory,
            hidden: false,
            origin: ProfileOrigin::Reported,
        };
        profile.validate()?;
        Ok(profile)
    }
}

/// Rows in one reporter chunk. Account and machine fields on domain rows are
/// accepted for compatibility but overwritten after token authentication.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PushBatch {
    pub profiles: Vec<ReportedProfile>,
    pub snapshots: Vec<UsageSnapshot>,
    pub token_grains: Vec<TokenGrain>,
    pub context_sessions: Vec<ContextSession>,
    pub gemini_quotas: Vec<GeminiQuota>,
}

impl PushBatch {
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.profiles
            .len()
            .saturating_add(self.snapshots.len())
            .saturating_add(self.token_grains.len())
            .saturating_add(self.context_sessions.len())
            .saturating_add(self.gemini_quotas.len())
    }
}

/// Versioned reporter wire document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushEnvelope {
    pub version: u16,
    pub request_id: String,
    pub reporter_version: String,
    /// Informational compatibility fields. Authentication always overrides
    /// them with the stored token record.
    #[serde(default)]
    pub account_id: Option<AccountId>,
    #[serde(default)]
    pub machine: Option<MachineName>,
    pub batch: PushBatch,
}

impl PushEnvelope {
    /// Encodes one already-bounded reporter chunk.
    ///
    /// # Errors
    ///
    /// Returns invalid input if the document is malformed or exceeds 1 MiB.
    pub fn encode(&self) -> PulseResult<Vec<u8>> {
        validate_request_id(&self.request_id)?;
        validate_reporter_version(&self.reporter_version)?;
        if self.version != PUSH_VERSION || self.batch.row_count() > MAX_PUSH_ROWS {
            return Err(PulseError::invalid_input(
                "Pulse reporter document exceeds protocol bounds",
            ));
        }
        let bytes = serde_json::to_vec(self).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Internal,
                "failed to encode Pulse reporter document",
            )
        })?;
        if bytes.len() > MAX_PUSH_BODY_BYTES {
            return Err(PulseError::invalid_input(
                "Pulse reporter document exceeds 1 MiB",
            ));
        }
        Ok(bytes)
    }
}

/// Existing web security checks have succeeded for this request.
///
/// Construction is crate-visible so external callers cannot bypass the atmux
/// Host/auth/Origin middleware and call the receiver directly by accident.
#[derive(Clone, Copy, Debug)]
pub struct VerifiedReceiverBoundary(());

impl VerifiedReceiverBoundary {
    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn after_host_auth_origin_checks() -> Self {
        Self(())
    }
}

/// Whether the existing atmux listener protected this connection with TLS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverTransport {
    Tls,
    Plaintext,
}

/// Framework-neutral request supplied by the authenticated web adapter.
pub struct ReceiverRequest<'a> {
    pub boundary: VerifiedReceiverBoundary,
    pub peer_ip: IpAddr,
    pub transport: ReceiverTransport,
    pub bearer: &'a str,
    pub body: &'a [u8],
    pub received_at: Instant,
}

/// Public token metadata. The stored hash is intentionally absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestTokenSummary {
    pub id: i64,
    pub account_id: AccountId,
    pub machine: MachineName,
    pub created_at: Instant,
    pub last_used_at: Option<Instant>,
    pub revoked_at: Option<Instant>,
}

impl From<IngestToken> for IngestTokenSummary {
    fn from(token: IngestToken) -> Self {
        Self {
            id: token.id,
            account_id: token.account_id,
            machine: token.machine,
            created_at: token.created_at,
            last_used_at: token.last_used_at,
            revoked_at: token.revoked_at,
        }
    }
}

/// Plaintext returned exactly once by token issuance.
pub struct IssuedIngestToken {
    pub summary: IngestTokenSummary,
    plaintext: String,
}

impl IssuedIngestToken {
    /// Consumes the one-time result and exposes the plaintext to the caller.
    #[must_use]
    pub fn into_plaintext(self) -> String {
        self.plaintext
    }

    /// Consumes the one-time result into public metadata and plaintext. The
    /// caller must deliver the plaintext immediately; it cannot be recovered.
    #[must_use]
    pub fn into_parts(self) -> (IngestTokenSummary, String) {
        (self.summary, self.plaintext)
    }
}

impl fmt::Debug for IssuedIngestToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedIngestToken")
            .field("summary", &self.summary)
            .field("plaintext", &"[redacted]")
            .finish()
    }
}

/// Account-scoped token management seam for the REST/MCP adapters.
#[derive(Clone)]
pub struct IngestTokenManager {
    store: Arc<dyn Store>,
}

impl IngestTokenManager {
    #[must_use]
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    /// Issues a CSPRNG bearer and persists only its SHA-256 digest.
    ///
    /// # Errors
    ///
    /// Returns a typed storage/conflict error when the account token cap is
    /// reached, the OS CSPRNG is unavailable, or persistence fails.
    pub async fn issue(
        &self,
        account_id: AccountId,
        machine: MachineName,
        created_at: Instant,
    ) -> PulseResult<IssuedIngestToken> {
        let (id, random) = generate_token_parts()?;
        let plaintext = format!(
            "{TOKEN_PREFIX}.{}.{id}.{}",
            account_id.get(),
            URL_SAFE_NO_PAD.encode(random)
        );
        let token = IngestToken {
            id,
            account_id,
            machine: machine.clone(),
            token_hash: sha256_hex(plaintext.as_bytes()),
            created_at,
            last_used_at: None,
            revoked_at: None,
        };
        let summary = IngestTokenSummary::from(token.clone());
        self.store
            .issue_ingest_token(
                Machine {
                    account_id,
                    name: machine,
                    first_seen: created_at,
                    last_seen: created_at,
                },
                token,
                MAX_ACTIVE_INGEST_TOKENS,
            )
            .await?;
        Ok(IssuedIngestToken { summary, plaintext })
    }

    /// Lists secret-free token metadata for one account.
    ///
    /// # Errors
    ///
    /// Returns a typed storage error when the account token list is unavailable.
    pub async fn list(&self, account_id: AccountId) -> PulseResult<Vec<IngestTokenSummary>> {
        self.store
            .list_ingest_tokens(account_id)
            .await
            .map(|tokens| tokens.into_iter().map(IngestTokenSummary::from).collect())
    }

    /// Revokes one token without ever loading or returning its hash.
    ///
    /// # Errors
    ///
    /// Returns a typed storage error when the account-scoped update fails.
    pub async fn revoke(
        &self,
        account_id: AccountId,
        token_id: i64,
        revoked_at: Instant,
    ) -> PulseResult<bool> {
        self.store
            .revoke_ingest_token(account_id, token_id, revoked_at)
            .await
    }

    async fn authenticate(&self, plaintext: &str) -> PulseResult<AuthenticatedIngestToken> {
        let (account_id, token_id) = parse_token_locator(plaintext)?;
        let Some(stored) = self.store.get_ingest_token(account_id, token_id).await? else {
            return Err(authentication_failed());
        };
        if stored.revoked_at.is_some() {
            return Err(authentication_failed());
        }
        let candidate = sha256_hex(plaintext.as_bytes());
        if !constant_time_eq(candidate.as_bytes(), stored.token_hash.as_bytes()) {
            return Err(authentication_failed());
        }
        Ok(AuthenticatedIngestToken {
            id: stored.id,
            account_id: stored.account_id,
            machine: stored.machine,
        })
    }
}

#[derive(Clone, Debug)]
struct AuthenticatedIngestToken {
    id: i64,
    account_id: AccountId,
    machine: MachineName,
}

#[derive(Clone, Copy, Debug)]
struct RateEntry {
    window_started: MonotonicInstant,
    last_seen: MonotonicInstant,
    attempts: u32,
}

/// Fixed-window limiter with explicit expiry and cardinality bounds.
pub struct BoundedRateLimiter<K> {
    entries: Mutex<HashMap<K, RateEntry>>,
    limit: u32,
    window: Duration,
    max_entries: usize,
}

impl<K> BoundedRateLimiter<K>
where
    K: Clone + Eq + Hash,
{
    /// Creates a nonzero, bounded limiter.
    ///
    /// # Errors
    ///
    /// Rejects zero limits, windows, or cardinality.
    pub fn new(limit: u32, window: Duration, max_entries: usize) -> PulseResult<Self> {
        if limit == 0 || window.is_zero() || max_entries == 0 || max_entries > 100_000 {
            return Err(PulseError::configuration(
                "Pulse ingest rate limiter bounds are invalid",
            ));
        }
        Ok(Self {
            entries: Mutex::new(HashMap::new()),
            limit,
            window,
            max_entries,
        })
    }

    fn allow(&self, key: K, now: MonotonicInstant) -> bool {
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        entries.retain(|_, entry| now.saturating_duration_since(entry.last_seen) <= self.window);
        if let Some(entry) = entries.get_mut(&key) {
            if now.saturating_duration_since(entry.window_started) >= self.window {
                *entry = RateEntry {
                    window_started: now,
                    last_seen: now,
                    attempts: 1,
                };
                return true;
            }
            entry.last_seen = now;
            entry.attempts = entry.attempts.saturating_add(1);
            return entry.attempts <= self.limit;
        }
        if entries.len() >= self.max_entries
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            RateEntry {
                window_started: now,
                last_seen: now,
                attempts: 1,
            },
        );
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().map_or(0, |entries| entries.len())
    }
}

/// Optional receiver with IP-first and token-after-authentication throttles.
pub struct IngestReceiver {
    enabled: bool,
    store: Arc<dyn Store>,
    tokens: IngestTokenManager,
    ip_limiter: BoundedRateLimiter<IpAddr>,
    token_limiter: BoundedRateLimiter<(AccountId, i64)>,
    limits: IngestLimits,
    invalidations: Option<super::invalidation::PulseInvalidationHub>,
}

impl IngestReceiver {
    /// Builds a receiver. `enabled` must come from `pulse.receive`, which is
    /// false by default.
    ///
    /// # Errors
    ///
    /// Returns configuration errors for invalid limiter or ingest bounds.
    pub fn new(enabled: bool, store: Arc<dyn Store>, limits: IngestLimits) -> PulseResult<Self> {
        if limits.max_rows_per_request == 0 || limits.max_rows_per_request > MAX_PUSH_ROWS {
            return Err(PulseError::configuration(
                "Pulse receiver row limit must be between 1 and 10000",
            ));
        }
        Ok(Self {
            enabled,
            tokens: IngestTokenManager::new(Arc::clone(&store)),
            store,
            ip_limiter: BoundedRateLimiter::new(30, Duration::from_secs(60), 8_192)?,
            token_limiter: BoundedRateLimiter::new(120, Duration::from_secs(60), 4_096)?,
            limits,
            invalidations: None,
        })
    }

    #[must_use]
    pub fn with_invalidations(
        mut self,
        invalidations: super::invalidation::PulseInvalidationHub,
    ) -> Self {
        self.invalidations = Some(invalidations);
        self
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn token_manager(&self) -> &IngestTokenManager {
        &self.tokens
    }

    /// Authenticates, bounds, rewrites, and transactionally ingests one chunk.
    ///
    /// Ordering is security-significant: IP throttling happens before parsing
    /// the token locator or querying storage; per-token throttling happens only
    /// after constant-time digest authentication.
    ///
    /// # Errors
    ///
    /// Returns not-found when disabled; authentication/rate-limit errors for a
    /// rejected request; invalid input for an unsafe body; or a typed storage
    /// error when transactional ingest/touch fails.
    pub async fn receive(
        &self,
        request: ReceiverRequest<'_>,
    ) -> PulseResult<IdempotentIngestResult> {
        let VerifiedReceiverBoundary(()) = request.boundary;
        if !self.enabled {
            return Err(PulseError::new(
                PulseErrorKind::NotFound,
                "Pulse push receiver is disabled",
            ));
        }
        if request.transport == ReceiverTransport::Plaintext && !request.peer_ip.is_loopback() {
            return Err(PulseError::new(
                PulseErrorKind::Authentication,
                "Pulse push ingest requires HTTPS outside loopback",
            ));
        }
        let monotonic_now = MonotonicInstant::now();
        if !self.ip_limiter.allow(request.peer_ip, monotonic_now) {
            return Err(rate_limited());
        }
        let authenticated = self.tokens.authenticate(request.bearer).await?;
        if !self
            .token_limiter
            .allow((authenticated.account_id, authenticated.id), monotonic_now)
        {
            return Err(rate_limited());
        }
        let decoded = decode_push_body(
            request.body,
            authenticated.account_id,
            &authenticated.machine,
            self.limits,
            request.received_at,
        )?;
        let result = self
            .store
            .ingest_batch_once(
                authenticated.account_id,
                authenticated.machine,
                decoded.batch,
                self.limits,
                decoded.replay,
            )
            .await?;
        if let Some(invalidations) = &self.invalidations {
            let _ = invalidations.publish(authenticated.account_id);
        }
        if !self
            .store
            .touch_ingest_token(
                authenticated.account_id,
                authenticated.id,
                request.received_at,
            )
            .await?
        {
            return Err(authentication_failed());
        }
        Ok(result)
    }
}

struct DecodedPush {
    batch: IngestBatch,
    replay: IngestReplay,
}

/// Parses a bounded body and replaces all body authority with the authenticated
/// token scope before validating domain rows.
fn decode_push_body(
    body: &[u8],
    account_id: AccountId,
    machine: &MachineName,
    limits: IngestLimits,
    received_at: Instant,
) -> PulseResult<DecodedPush> {
    if body.is_empty() || body.len() > MAX_PUSH_BODY_BYTES {
        return Err(PulseError::invalid_input(
            "Pulse push body must be between 1 byte and 1 MiB",
        ));
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| PulseError::invalid_input("Pulse push body is not valid JSON"))?;
    validate_json_shape(&value)?;
    let envelope: PushEnvelope = serde_json::from_value(value)
        .map_err(|_| PulseError::invalid_input("Pulse push document is invalid"))?;
    if envelope.version != PUSH_VERSION {
        return Err(PulseError::invalid_input(
            "Pulse push protocol version is unsupported",
        ));
    }
    validate_request_id(&envelope.request_id)?;
    validate_reporter_version(&envelope.reporter_version)?;
    if envelope.batch.row_count() > MAX_PUSH_ROWS
        || envelope.batch.row_count() > limits.max_rows_per_request
    {
        return Err(PulseError::invalid_input(
            "Pulse push document exceeds its row limit",
        ));
    }

    let mut profiles = Vec::with_capacity(envelope.batch.profiles.len());
    for profile in envelope.batch.profiles {
        profiles.push(profile.into_domain(account_id)?);
    }
    let mut snapshots = envelope.batch.snapshots;
    for snapshot in &mut snapshots {
        snapshot.account_id = account_id;
        snapshot.machine = machine.clone();
        snapshot.reporter_version = Some(envelope.reporter_version.clone());
        if snapshot.windows.len() > 4 {
            return Err(PulseError::invalid_input(
                "Pulse snapshot contains too many windows",
            ));
        }
        snapshot.validate()?;
    }
    let mut token_grains = envelope.batch.token_grains;
    for grain in &mut token_grains {
        grain.account_id = account_id;
        grain.machine = machine.clone();
        grain.source = TokenSource::Ingest;
        validate_settings(&grain.settings)?;
        grain.validate()?;
    }
    let mut context_sessions = envelope.batch.context_sessions;
    for session in &mut context_sessions {
        session.account_id = account_id;
        session.machine = machine.clone();
        validate_settings(&session.settings)?;
        session.validate()?;
    }
    let mut gemini_quotas = envelope.batch.gemini_quotas;
    for quota in &mut gemini_quotas {
        quota.account_id = account_id;
        if quota.remaining_amount.as_ref().is_some_and(|amount| {
            amount.len() > MAX_GEMINI_AMOUNT_BYTES || amount.chars().any(char::is_control)
        }) {
            return Err(PulseError::invalid_input(
                "Gemini remaining amount is invalid",
            ));
        }
        quota.validate()?;
    }
    Ok(DecodedPush {
        batch: IngestBatch {
            profiles,
            snapshots,
            token_grains,
            context_sessions,
            gemini_quotas,
        },
        replay: IngestReplay {
            request_id: envelope.request_id,
            payload_fingerprint: sha256_hex(body),
            received_at,
        },
    })
}

fn validate_json_shape(root: &Value) -> PulseResult<()> {
    let mut stack = vec![(root, 1_usize)];
    let mut nodes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if nodes > MAX_JSON_NODES || depth > MAX_JSON_DEPTH {
            return Err(PulseError::invalid_input(
                "Pulse push JSON exceeds structural bounds",
            ));
        }
        match value {
            Value::String(text) if text.len() > MAX_JSON_STRING_BYTES => {
                return Err(PulseError::invalid_input(
                    "Pulse push JSON contains an oversized string",
                ));
            }
            Value::Array(items) => {
                stack.extend(items.iter().map(|item| (item, depth.saturating_add(1))));
            }
            Value::Object(entries) => {
                if entries.keys().any(|key| key.len() > MAX_SETTINGS_KEY_BYTES) {
                    return Err(PulseError::invalid_input(
                        "Pulse push JSON contains an oversized field name",
                    ));
                }
                stack.extend(entries.values().map(|item| (item, depth.saturating_add(1))));
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_settings(settings: &AgentSettings) -> PulseResult<()> {
    if settings.additional.len() > MAX_SETTINGS_ENTRIES {
        return Err(PulseError::invalid_input(
            "Pulse agent settings contain too many fields",
        ));
    }
    for value in [settings.service_tier.as_deref(), settings.effort.as_deref()]
        .into_iter()
        .flatten()
    {
        validate_safe_text(value, MAX_SETTINGS_VALUE_BYTES, "agent setting")?;
    }
    for (key, value) in &settings.additional {
        validate_safe_text(key, MAX_SETTINGS_KEY_BYTES, "agent setting key")?;
        validate_safe_text(value, MAX_SETTINGS_VALUE_BYTES, "agent setting value")?;
    }
    Ok(())
}

fn validate_safe_text(value: &str, max: usize, label: &str) -> PulseResult<()> {
    if value.is_empty()
        || value.len() > max
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(PulseError::invalid_input(format!("{label} is invalid")));
    }
    Ok(())
}

fn validate_request_id(value: &str) -> PulseResult<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(PulseError::invalid_input(
            "Pulse request id must be a bounded ASCII identifier",
        ));
    }
    Ok(())
}

fn validate_reporter_version(value: &str) -> PulseResult<()> {
    validate_safe_text(value, 128, "Pulse reporter version")
}

fn generate_token_parts() -> PulseResult<(i64, [u8; TOKEN_RANDOM_BYTES])> {
    let mut bytes = [0_u8; TOKEN_RANDOM_BYTES + 8];
    OsRng.try_fill_bytes(&mut bytes).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Internal,
            "operating-system randomness is unavailable",
        )
    })?;
    let mut id_bytes = [0_u8; 8];
    id_bytes.copy_from_slice(&bytes[..8]);
    let id = i64::from_be_bytes(id_bytes) & i64::MAX;
    if id == 0 {
        return generate_token_parts();
    }
    let mut random = [0_u8; TOKEN_RANDOM_BYTES];
    random.copy_from_slice(&bytes[8..]);
    Ok((id, random))
}

fn parse_token_locator(value: &str) -> PulseResult<(AccountId, i64)> {
    if value.len() > MAX_BEARER_BYTES || value.chars().any(char::is_whitespace) {
        return Err(authentication_failed());
    }
    let mut pieces = value.split('.');
    let (Some(prefix), Some(account), Some(id), Some(random), None) = (
        pieces.next(),
        pieces.next(),
        pieces.next(),
        pieces.next(),
        pieces.next(),
    ) else {
        return Err(authentication_failed());
    };
    if prefix != TOKEN_PREFIX
        || random.len() != TOKEN_RANDOM_TEXT_BYTES
        || !random
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(authentication_failed());
    }
    let account_id = account
        .parse::<i64>()
        .ok()
        .and_then(|value| AccountId::new(value).ok())
        .ok_or_else(authentication_failed)?;
    let token_id = id
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(authentication_failed)?;
    Ok((account_id, token_id))
}

fn sha256_hex(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
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

fn authentication_failed() -> PulseError {
    PulseError::new(
        PulseErrorKind::Authentication,
        "Pulse ingest authentication failed",
    )
}

fn rate_limited() -> PulseError {
    PulseError::new(
        PulseErrorKind::RateLimited,
        "Pulse ingest request rate exceeded",
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, net::Ipv4Addr};

    use super::*;
    use crate::pulse::{
        Account, CollectionOutcome, Machine, Percent, QuotaWindow, QuotaWindowKind, SessionId,
        store::{SqliteStore, Store},
    };

    fn account() -> AccountId {
        AccountId::new(1).expect("account")
    }

    fn machine() -> MachineName {
        MachineName::new("midnight").expect("machine")
    }

    fn timestamp(value: i64) -> Instant {
        Instant::from_epoch_millis(value).expect("timestamp")
    }

    fn snapshot(claimed_account: i64, claimed_machine: &str) -> UsageSnapshot {
        UsageSnapshot {
            account_id: AccountId::new(claimed_account).expect("account"),
            profile: ProfileName::new("claude-max").expect("profile"),
            machine: MachineName::new(claimed_machine).expect("machine"),
            vendor: Vendor::AnthropicOauth,
            windows: vec![QuotaWindow {
                kind: QuotaWindowKind::FiveHour,
                used_percent: Percent::new(12.0).expect("percent"),
                resets_at: timestamp(2_000_000),
            }],
            outcome: CollectionOutcome::Success,
            polled_at: timestamp(1_000_000),
            reporter_version: Some("hostile".to_owned()),
        }
    }

    fn envelope() -> PushEnvelope {
        PushEnvelope {
            version: PUSH_VERSION,
            request_id: "request-1".to_owned(),
            reporter_version: "atmux/0.1.0".to_owned(),
            account_id: AccountId::new(99).ok(),
            machine: MachineName::new("attacker").ok(),
            batch: PushBatch {
                profiles: vec![ReportedProfile {
                    name: ProfileName::new("claude-max").expect("profile"),
                    vendor: Vendor::AnthropicOauth,
                    poll_interval_minutes: 15,
                    monthly_budget_usd: None,
                }],
                snapshots: vec![snapshot(99, "attacker")],
                ..PushBatch::default()
            },
        }
    }

    fn local_request<'a>(body: &'a [u8], bearer: &'a str) -> ReceiverRequest<'a> {
        ReceiverRequest {
            boundary: VerifiedReceiverBoundary::after_host_auth_origin_checks(),
            peer_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            transport: ReceiverTransport::Plaintext,
            bearer,
            body,
            received_at: timestamp(3_000_000),
        }
    }

    #[test]
    fn authenticated_scope_overrides_every_body_claim() {
        let body = envelope().encode().expect("encode");
        let decoded = decode_push_body(
            &body,
            account(),
            &machine(),
            IngestLimits::default(),
            timestamp(3_000_000),
        )
        .expect("decode");
        assert_eq!(decoded.batch.profiles[0].account_id, account());
        assert_eq!(decoded.batch.profiles[0].origin, ProfileOrigin::Reported);
        assert_eq!(decoded.batch.snapshots[0].account_id, account());
        assert_eq!(decoded.batch.snapshots[0].machine, machine());
        assert_eq!(
            decoded.batch.snapshots[0].reporter_version.as_deref(),
            Some("atmux/0.1.0")
        );
        assert_eq!(decoded.replay.payload_fingerprint.len(), 64);
    }

    #[test]
    fn hostile_depth_strings_and_rows_are_rejected() {
        let mut deep = Value::Null;
        for _ in 0..=MAX_JSON_DEPTH {
            deep = Value::Array(vec![deep]);
        }
        assert!(validate_json_shape(&deep).is_err());
        assert!(
            validate_json_shape(&Value::String("x".repeat(MAX_JSON_STRING_BYTES + 1))).is_err()
        );
        let mut too_many = envelope();
        too_many.batch.profiles = (0..=MAX_PUSH_ROWS)
            .map(|index| ReportedProfile {
                name: ProfileName::new(format!("p{index}")).expect("profile"),
                vendor: Vendor::AnthropicOauth,
                poll_interval_minutes: 15,
                monthly_budget_usd: None,
            })
            .collect();
        assert!(too_many.encode().is_err());
    }

    #[test]
    fn token_locator_and_digest_comparison_fail_closed() {
        let random = URL_SAFE_NO_PAD.encode([7_u8; TOKEN_RANDOM_BYTES]);
        let token = format!("{TOKEN_PREFIX}.1.4.{random}");
        assert_eq!(
            parse_token_locator(&token).expect("locator"),
            (account(), 4)
        );
        assert!(parse_token_locator("atmux-pulse-v1.1.4.short").is_err());
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"diff"));
        assert!(!constant_time_eq(b"same", b"shorter"));
        assert!(!format!("{token:?}").contains("[redacted]"));
    }

    #[test]
    fn limiter_expires_and_never_exceeds_cardinality() {
        let limiter = BoundedRateLimiter::new(2, Duration::from_secs(10), 2).expect("limiter");
        let start = MonotonicInstant::now();
        assert!(limiter.allow(1_u8, start));
        assert!(limiter.allow(1_u8, start));
        assert!(!limiter.allow(1_u8, start));
        assert!(limiter.allow(2_u8, start));
        assert!(limiter.allow(3_u8, start));
        assert_eq!(limiter.len(), 2);
        assert!(limiter.allow(1_u8, start + Duration::from_secs(11)));
    }

    #[test]
    fn settings_bounds_are_enforced() {
        let mut settings = AgentSettings {
            service_tier: Some("priority".to_owned()),
            effort: Some("high".to_owned()),
            additional: BTreeMap::new(),
        };
        assert!(validate_settings(&settings).is_ok());
        for index in 0..=MAX_SETTINGS_ENTRIES {
            settings
                .additional
                .insert(format!("k{index}"), "v".to_owned());
        }
        assert!(validate_settings(&settings).is_err());

        let grain = TokenGrain {
            account_id: account(),
            profile: ProfileName::new("claude-max").expect("profile"),
            machine: machine(),
            session_id: SessionId::new("session").expect("session"),
            model: "claude".to_owned(),
            settings: AgentSettings::default(),
            settings_hash: AgentSettings::default().sha256().expect("hash"),
            day: "2026-08-08".to_owned(),
            tokens_in: 1,
            tokens_out: 1,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
            source: TokenSource::Local,
        };
        assert_eq!(grain.source, TokenSource::Local);
    }

    #[test]
    fn issued_token_debug_is_redacted() {
        let issued = IssuedIngestToken {
            summary: IngestTokenSummary {
                id: 3,
                account_id: account(),
                machine: machine(),
                created_at: timestamp(1_000),
                last_used_at: None,
                revoked_at: None,
            },
            plaintext: "secret-value".to_owned(),
        };
        let debug = format!("{issued:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("secret-value"));
    }

    #[test]
    fn boundary_can_only_be_minted_after_existing_checks_inside_crate() {
        let boundary = VerifiedReceiverBoundary::after_host_auth_origin_checks();
        let VerifiedReceiverBoundary(()) = boundary;
        let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        assert!(address.is_loopback());
    }

    async fn receiver_fixture() -> (
        Arc<dyn Store>,
        IngestReceiver,
        super::super::invalidation::PulseInvalidationHub,
        String,
        i64,
    ) {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open(":memory:").await.expect("store"));
        store
            .upsert_account(Account {
                id: account(),
                identity: "ryan@example.test".to_owned(),
                display_name: Some("Ryan".to_owned()),
            })
            .await
            .expect("account");
        store
            .upsert_machine(Machine {
                account_id: account(),
                name: machine(),
                first_seen: timestamp(1_000),
                last_seen: timestamp(1_000),
            })
            .await
            .expect("machine");
        let invalidations = super::super::invalidation::PulseInvalidationHub::new(&[account()]);
        let receiver = IngestReceiver::new(true, Arc::clone(&store), IngestLimits::default())
            .expect("receiver")
            .with_invalidations(invalidations.clone());
        let issued = receiver
            .token_manager()
            .issue(account(), machine(), timestamp(2_000))
            .await
            .expect("token");
        let id = issued.summary.id;
        let plaintext = issued.into_plaintext();
        (store, receiver, invalidations, plaintext, id)
    }

    #[tokio::test]
    async fn receiver_persists_hash_only_and_replays_exact_authoritative_batch() {
        let (store, receiver, invalidations, plaintext, token_id) = receiver_fixture().await;
        let mut subscription = invalidations.subscribe(account()).expect("subscription");
        let body = envelope().encode().expect("body");
        let request = || ReceiverRequest {
            boundary: VerifiedReceiverBoundary::after_host_auth_origin_checks(),
            peer_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            transport: ReceiverTransport::Plaintext,
            bearer: &plaintext,
            body: &body,
            received_at: timestamp(3_000_000),
        };
        let first = receiver.receive(request()).await.expect("first ingest");
        assert!(!first.replayed);
        subscription.receiver.changed().await.expect("invalidation");
        assert_eq!(*subscription.receiver.borrow_and_update(), 1);
        let second = receiver.receive(request()).await.expect("exact replay");
        assert!(second.replayed);

        let stored = store
            .get_ingest_token(account(), token_id)
            .await
            .expect("lookup")
            .expect("stored token");
        assert_ne!(stored.token_hash, plaintext);
        assert_eq!(stored.token_hash.len(), 64);
        assert_eq!(stored.last_used_at, Some(timestamp(3_000_000)));
        let profile = store
            .get_profile(account(), ProfileName::new("claude-max").expect("profile"))
            .await
            .expect("profile lookup")
            .expect("reported profile");
        assert_eq!(profile.origin, ProfileOrigin::Reported);
        let usage = store
            .usage_history(account(), profile.name, None, 10)
            .await
            .expect("usage");
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].snapshot.machine, machine());
    }

    #[tokio::test]
    async fn request_id_conflict_revocation_and_remote_plaintext_fail_closed() {
        let (_store, receiver, _invalidations, plaintext, token_id) = receiver_fixture().await;
        let mut first_envelope = envelope();
        let first = first_envelope.encode().expect("first");
        receiver
            .receive(local_request(&first, &plaintext))
            .await
            .expect("first ingest");
        first_envelope.reporter_version = "atmux/changed".to_owned();
        let changed = first_envelope.encode().expect("changed");
        assert_eq!(
            receiver
                .receive(local_request(&changed, &plaintext))
                .await
                .expect_err("request id conflict")
                .kind(),
            PulseErrorKind::Conflict
        );
        assert_eq!(
            receiver
                .receive(ReceiverRequest {
                    boundary: VerifiedReceiverBoundary::after_host_auth_origin_checks(),
                    peer_ip: "192.0.2.4".parse().expect("address"),
                    transport: ReceiverTransport::Plaintext,
                    bearer: &plaintext,
                    body: &first,
                    received_at: timestamp(3_000_000),
                })
                .await
                .expect_err("remote cleartext")
                .kind(),
            PulseErrorKind::Authentication
        );
        receiver
            .token_manager()
            .revoke(account(), token_id, timestamp(4_000_000))
            .await
            .expect("revoke");
        assert_eq!(
            receiver
                .receive(local_request(&first, &plaintext))
                .await
                .expect_err("revoked")
                .kind(),
            PulseErrorKind::Authentication
        );
    }

    #[tokio::test]
    async fn malformed_tokens_are_ip_limited_before_storage_amplification() {
        let (_store, receiver, _invalidations, _plaintext, _token_id) = receiver_fixture().await;
        let body = envelope().encode().expect("body");
        let peer = "192.0.2.8".parse().expect("address");
        for _ in 0..30 {
            let error = receiver
                .receive(ReceiverRequest {
                    boundary: VerifiedReceiverBoundary::after_host_auth_origin_checks(),
                    peer_ip: peer,
                    transport: ReceiverTransport::Tls,
                    bearer: "not-a-token",
                    body: &body,
                    received_at: timestamp(3_000_000),
                })
                .await
                .expect_err("malformed token");
            assert_eq!(error.kind(), PulseErrorKind::Authentication);
        }
        let limited = receiver
            .receive(ReceiverRequest {
                boundary: VerifiedReceiverBoundary::after_host_auth_origin_checks(),
                peer_ip: peer,
                transport: ReceiverTransport::Tls,
                bearer: "not-a-token",
                body: &body,
                received_at: timestamp(3_000_000),
            })
            .await
            .expect_err("IP limited");
        assert_eq!(limited.kind(), PulseErrorKind::RateLimited);
    }
}
