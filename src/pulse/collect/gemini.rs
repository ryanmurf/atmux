//! Gemini consumer quota collection with in-memory OAuth refresh/cache.

use std::{
    env, fmt,
    path::{Path, PathBuf},
    time::{Duration, Instant as MonotonicInstant},
};

use hyper::{Method, StatusCode, header};
use serde::Deserialize;
use tokio::sync::Mutex;

use super::{HttpsJsonClient, read_regular_bounded};
use crate::pulse::{
    AccountId, CollectionOutcome, Fraction, GeminiQuota, Instant, PulseError, PulseResult,
};

const QUOTA_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const MAX_BUCKETS: usize = 128;
const REQUEST_THROTTLE: Duration = Duration::from_secs(30);
const TOKEN_EXPIRY_MARGIN_MS: i64 = 60 * 1_000;

/// Expected scheduler cadence. The independent 30-second request throttle is
/// only a safety floor, not the ordinary collection schedule.
pub const SCHEDULE_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Explicit Gemini collector configuration. OAuth application credentials must be
/// injected through bounded, externally referenced environment variables; atmux
/// has no compiled-in Google client identity or secret.
/// Access and refresh tokens are read only from the referenced regular file.
#[derive(Clone)]
pub struct GeminiConfig {
    enabled: bool,
    oauth_path: PathBuf,
    client_id: String,
    client_secret: String,
}

impl fmt::Debug for GeminiConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeminiConfig")
            .field("enabled", &self.enabled)
            .field("oauth_path", &self.oauth_path)
            .field("client_id", &"[externally supplied]")
            .field("client_secret", &"[externally supplied]")
            .finish()
    }
}

impl GeminiConfig {
    /// Builds a collector from explicitly supplied OAuth application values.
    ///
    /// # Errors
    ///
    /// Returns a configuration error without including either supplied value.
    pub fn new(oauth_path: PathBuf, client_id: String, client_secret: String) -> PulseResult<Self> {
        let config = Self {
            enabled: true,
            oauth_path,
            client_id,
            client_secret,
        };
        config.validate()?;
        Ok(config)
    }

    /// Resolves the OAuth application values from two explicit environment
    /// references. Neither environment names nor values appear in errors.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when a reference is missing or resolves
    /// to a missing, non-Unicode, or invalid value.
    pub fn from_environment(
        oauth_path: PathBuf,
        client_id_env: Option<&str>,
        client_secret_env: Option<&str>,
    ) -> PulseResult<Self> {
        Self::from_environment_with(oauth_path, client_id_env, client_secret_env, |name| {
            env::var(name).ok()
        })
    }

    fn from_environment_with<F>(
        oauth_path: PathBuf,
        client_id_env: Option<&str>,
        client_secret_env: Option<&str>,
        mut resolve: F,
    ) -> PulseResult<Self>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let client_id_env = client_id_env.ok_or_else(|| {
            PulseError::configuration("Gemini OAuth client id environment reference is missing")
        })?;
        let client_secret_env = client_secret_env.ok_or_else(|| {
            PulseError::configuration("Gemini OAuth client secret environment reference is missing")
        })?;
        let client_id = resolve(client_id_env).ok_or_else(|| {
            PulseError::configuration("Gemini OAuth client id environment variable is unavailable")
        })?;
        let client_secret = resolve(client_secret_env).ok_or_else(|| {
            PulseError::configuration(
                "Gemini OAuth client secret environment variable is unavailable",
            )
        })?;
        Self::new(oauth_path, client_id, client_secret)
    }

    /// # Errors
    ///
    /// Returns a configuration error for a relative credential path or invalid
    /// OAuth application configuration.
    pub fn validate(&self) -> PulseResult<()> {
        if !self.oauth_path.is_absolute() {
            return Err(PulseError::configuration(
                "Gemini OAuth credential path must be absolute",
            ));
        }
        for (name, value) in [
            ("Gemini OAuth client id", self.client_id.as_str()),
            ("Gemini OAuth client secret", self.client_secret.as_str()),
        ] {
            if value.is_empty()
                || value.len() > 512
                || value.trim() != value
                || value.chars().any(char::is_control)
            {
                return Err(PulseError::configuration(format!("{name} is invalid")));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct OAuthCredentials {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expiry: Option<serde_json::Value>,
    expiry_date: Option<serde_json::Value>,
}

fn read_oauth_credentials(path: &Path) -> PulseResult<Option<OAuthCredentials>> {
    let text = match read_regular_bounded(path, MAX_CREDENTIAL_BYTES) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(PulseError::invalid_input(
                "Gemini OAuth credential file was not a bounded regular file",
            ));
        }
    };
    let credentials = serde_json::from_str::<OAuthCredentials>(&text)
        .map_err(|_| PulseError::invalid_input("Gemini OAuth credential shape was invalid"))?;
    for token in [
        credentials.access_token.as_deref(),
        credentials.refresh_token.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if token.is_empty() || token.len() > 16 * 1024 || token.chars().any(char::is_control) {
            return Err(PulseError::invalid_input(
                "Gemini OAuth credential value was invalid",
            ));
        }
    }
    Ok(Some(credentials))
}

fn parse_expiry_ms(value: Option<&serde_json::Value>) -> i64 {
    let Some(value) = value else {
        return 0;
    };
    let numeric = value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse::<i64>().ok()));
    if let Some(numeric) = numeric {
        return if numeric < 10_000_000_000 {
            numeric.saturating_mul(1_000)
        } else {
            numeric
        };
    }
    value
        .as_str()
        .and_then(|value| Instant::from_iso8601(value).ok())
        .map_or(0, Instant::epoch_millis)
}

#[derive(Debug, Deserialize)]
struct QuotaEnvelope {
    buckets: Vec<QuotaBucket>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotaBucket {
    model_id: String,
    remaining_fraction: f64,
    remaining_amount: Option<String>,
    reset_time: Option<String>,
}

/// Parses and validates every model bucket. A malformed bucket rejects the
/// whole response so partial provider changes cannot silently skew the UI.
///
/// # Errors
///
/// Returns an invalid-response error for an oversized/malformed document,
/// duplicate/invalid models, out-of-range fractions, or invalid resets.
pub fn parse_quota_response(
    account_id: AccountId,
    body: &[u8],
    collected_at: Instant,
) -> PulseResult<Vec<GeminiQuota>> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(PulseError::invalid_input(
            "Gemini quota response exceeded its size bound",
        ));
    }
    let envelope: QuotaEnvelope = serde_json::from_slice(body)
        .map_err(|_| PulseError::invalid_input("Gemini quota response shape was invalid"))?;
    if envelope.buckets.len() > MAX_BUCKETS {
        return Err(PulseError::invalid_input(
            "Gemini quota response contained too many buckets",
        ));
    }
    let mut models = std::collections::BTreeSet::new();
    let mut quotas = Vec::with_capacity(envelope.buckets.len());
    for bucket in envelope.buckets {
        if !models.insert(bucket.model_id.clone()) {
            return Err(PulseError::invalid_input(
                "Gemini quota response contained a duplicate model",
            ));
        }
        if bucket
            .remaining_amount
            .as_ref()
            .is_some_and(|value| value.len() > 128 || value.chars().any(char::is_control))
        {
            return Err(PulseError::invalid_input(
                "Gemini remaining amount was invalid",
            ));
        }
        let remaining_amount = bucket.remaining_amount;
        let resets_at = bucket
            .reset_time
            .as_deref()
            .map(Instant::from_iso8601)
            .transpose()?;
        let quota = GeminiQuota {
            account_id,
            model_id: bucket.model_id,
            remaining_fraction: Fraction::new(bucket.remaining_fraction)?,
            remaining_amount,
            resets_at,
            collected_at,
        };
        quota.validate()?;
        quotas.push(quota);
    }
    Ok(quotas)
}

/// Secret-free result of one Gemini collection attempt.
#[derive(Clone, Debug, PartialEq)]
pub struct GeminiCollection {
    pub outcome: CollectionOutcome,
    pub quotas: Vec<GeminiQuota>,
}

struct CachedAccessToken {
    value: String,
    expires_at_ms: i64,
    refresh_token: String,
}

#[derive(Default)]
struct CollectorState {
    last_quota_request: Option<MonotonicInstant>,
    last_refresh_attempt: Option<MonotonicInstant>,
    cached: Option<CachedAccessToken>,
}

/// OAuth-aware Gemini HTTPS adapter. Refresh tokens and cached access tokens
/// remain in memory and are never serialized or persisted by atmux.
pub struct GeminiCollector {
    config: GeminiConfig,
    client: HttpsJsonClient,
    state: Mutex<CollectorState>,
}

impl fmt::Debug for GeminiCollector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeminiCollector")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl GeminiCollector {
    /// # Errors
    ///
    /// Returns a configuration error for invalid config or missing TLS roots.
    pub fn new(config: GeminiConfig) -> PulseResult<Self> {
        config.validate()?;
        Ok(Self {
            config,
            client: HttpsJsonClient::new(MAX_RESPONSE_BYTES)?,
            state: Mutex::new(CollectorState::default()),
        })
    }

    pub async fn collect(&self, account_id: AccountId, collected_at: Instant) -> GeminiCollection {
        if !self.config.enabled {
            return collection(
                CollectionOutcome::Disabled {
                    code: "gemini_disabled".to_owned(),
                },
                Vec::new(),
            );
        }
        let credentials = match read_oauth_credentials(&self.config.oauth_path) {
            Ok(Some(credentials)) => credentials,
            Ok(None) => {
                return collection(
                    CollectionOutcome::Disabled {
                        code: "gemini_credentials_missing".to_owned(),
                    },
                    Vec::new(),
                );
            }
            Err(_) => {
                return collection(
                    CollectionOutcome::InvalidResponse {
                        code: "gemini_credentials_invalid".to_owned(),
                    },
                    Vec::new(),
                );
            }
        };
        let mut state = self.state.lock().await;
        if state
            .last_quota_request
            .is_some_and(|last| last.elapsed() < REQUEST_THROTTLE)
        {
            return collection(
                CollectionOutcome::Disabled {
                    code: "gemini_request_throttled".to_owned(),
                },
                Vec::new(),
            );
        }
        let access_token = match self
            .access_token(&credentials, &mut state, collected_at)
            .await
        {
            Ok(Some(token)) => token,
            Ok(None) => {
                return collection(
                    CollectionOutcome::AuthenticationFailed {
                        code: "gemini_token_unavailable".to_owned(),
                    },
                    Vec::new(),
                );
            }
            Err(_) => {
                return collection(
                    CollectionOutcome::AuthenticationFailed {
                        code: "gemini_refresh_failed".to_owned(),
                    },
                    Vec::new(),
                );
            }
        };
        state.last_quota_request = Some(MonotonicInstant::now());
        let response = self
            .client
            .request(
                Method::POST,
                QUOTA_ENDPOINT,
                &[(
                    header::AUTHORIZATION.as_str(),
                    format!("Bearer {access_token}"),
                )],
                b"{}".to_vec(),
                Some("application/json"),
            )
            .await;
        drop(state);
        quota_collection(account_id, collected_at, response)
    }

    async fn access_token(
        &self,
        credentials: &OAuthCredentials,
        state: &mut CollectorState,
        now: Instant,
    ) -> PulseResult<Option<String>> {
        let expires_at = parse_expiry_ms(
            credentials
                .expiry_date
                .as_ref()
                .or(credentials.expiry.as_ref()),
        );
        if let Some(access_token) = &credentials.access_token
            && (expires_at == 0
                || expires_at > now.epoch_millis().saturating_add(TOKEN_EXPIRY_MARGIN_MS))
        {
            return Ok(Some(access_token.clone()));
        }
        let Some(refresh_token) = &credentials.refresh_token else {
            return Ok(credentials.access_token.clone());
        };
        if let Some(cached) = &state.cached
            && cached.refresh_token == *refresh_token
            && cached.expires_at_ms > now.epoch_millis().saturating_add(TOKEN_EXPIRY_MARGIN_MS)
        {
            return Ok(Some(cached.value.clone()));
        }
        if state
            .last_refresh_attempt
            .is_some_and(|last| last.elapsed() < REQUEST_THROTTLE)
        {
            return Ok(None);
        }
        state.last_refresh_attempt = Some(MonotonicInstant::now());
        let response = self
            .client
            .request(
                Method::POST,
                TOKEN_ENDPOINT,
                &[],
                refresh_form(&self.config, refresh_token),
                Some("application/x-www-form-urlencoded"),
            )
            .await?;
        if response.status != StatusCode::OK {
            return Ok(None);
        }
        let token: RefreshResponse = serde_json::from_slice(&response.body)
            .map_err(|_| PulseError::invalid_input("Gemini refresh response shape was invalid"))?;
        if token.access_token.is_empty()
            || token.access_token.len() > 16 * 1024
            || token.access_token.chars().any(char::is_control)
        {
            return Ok(None);
        }
        let lifetime_ms =
            i64::from(token.expires_in.unwrap_or(3_600).clamp(60, 86_400)).saturating_mul(1_000);
        state.cached = Some(CachedAccessToken {
            value: token.access_token.clone(),
            expires_at_ms: now.epoch_millis().saturating_add(lifetime_ms),
            refresh_token: refresh_token.clone(),
        });
        Ok(Some(token.access_token))
    }
}

fn quota_collection(
    account_id: AccountId,
    collected_at: Instant,
    response: PulseResult<super::HttpResponse>,
) -> GeminiCollection {
    match response {
        Ok(response) if response.status == StatusCode::OK => {
            match parse_quota_response(account_id, &response.body, collected_at) {
                Ok(quotas) => collection(CollectionOutcome::Success, quotas),
                Err(_) => collection(
                    CollectionOutcome::InvalidResponse {
                        code: "gemini_quota_response_invalid".to_owned(),
                    },
                    Vec::new(),
                ),
            }
        }
        Ok(response)
            if matches!(
                response.status,
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) =>
        {
            collection(
                CollectionOutcome::AuthenticationFailed {
                    code: "gemini_auth_rejected".to_owned(),
                },
                Vec::new(),
            )
        }
        Ok(response) if response.status == StatusCode::TOO_MANY_REQUESTS => {
            let retry_at = retry_at_from_headers(&response.headers, collected_at);
            collection(CollectionOutcome::RateLimited { retry_at }, Vec::new())
        }
        Ok(_) | Err(_) => collection(
            CollectionOutcome::Unavailable {
                code: "gemini_upstream_unavailable".to_owned(),
            },
            Vec::new(),
        ),
    }
}

fn retry_at_from_headers(headers: &[(String, String)], now: Instant) -> Option<Instant> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            if name.eq_ignore_ascii_case("retry-after") {
                let seconds = value.parse::<i64>().ok()?;
                let millis = seconds.checked_mul(1_000)?;
                return Instant::from_epoch_millis(now.epoch_millis().checked_add(millis)?).ok();
            }
            if name.eq_ignore_ascii_case("x-ratelimit-reset") {
                let numeric = value.parse::<i64>().ok()?;
                let millis = if numeric.unsigned_abs() < 100_000_000_000 {
                    numeric.checked_mul(1_000)?
                } else {
                    numeric
                };
                return Instant::from_epoch_millis(millis).ok();
            }
            None
        })
        .filter(|candidate| *candidate > now)
        .min()
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    expires_in: Option<u32>,
}

fn refresh_form(config: &GeminiConfig, refresh_token: &str) -> Vec<u8> {
    [
        ("client_id", config.client_id.as_str()),
        ("client_secret", config.client_secret.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ]
    .into_iter()
    .map(|(key, value)| format!("{}={}", form_encode(key), form_encode(value)))
    .collect::<Vec<_>>()
    .join("&")
    .into_bytes()
}

fn form_encode(value: &str) -> String {
    use fmt::Write as _;

    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            write!(output, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    output
}

fn collection(outcome: CollectionOutcome, quotas: Vec<GeminiQuota>) -> GeminiCollection {
    GeminiCollection { outcome, quotas }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_client_configuration_and_refresh_form_are_explicit_but_redacted() {
        let client_id = "fixture-client-id.apps.example.invalid";
        let client_secret = "fixture-client-secret-canary";
        let config = GeminiConfig::from_environment_with(
            PathBuf::from("/tmp/oauth-fixture.json"),
            Some("ATMUX_GEMINI_OAUTH_CLIENT_ID"),
            Some("ATMUX_GEMINI_OAUTH_CLIENT_SECRET"),
            |name| match name {
                "ATMUX_GEMINI_OAUTH_CLIENT_ID" => Some(client_id.to_owned()),
                "ATMUX_GEMINI_OAUTH_CLIENT_SECRET" => Some(client_secret.to_owned()),
                _ => None,
            },
        )
        .expect("injected collector config");
        let body = String::from_utf8(refresh_form(&config, "fixture refresh/value")).unwrap();
        assert!(body.contains(&format!("client_id={}", form_encode(client_id))));
        assert!(body.contains(&format!("client_secret={}", form_encode(client_secret))));
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=fixture%20refresh%2Fvalue"));
        let debug = format!("{config:?}");
        assert!(!debug.contains(client_id));
        assert!(!debug.contains(client_secret));
        assert_eq!(SCHEDULE_INTERVAL, Duration::from_secs(1_800));
        assert_eq!(REQUEST_THROTTLE, Duration::from_secs(30));
    }

    #[test]
    fn environment_injection_fails_closed_without_leaking_values() {
        let missing = GeminiConfig::from_environment_with(
            PathBuf::from("/tmp/oauth-fixture.json"),
            Some("ATMUX_GEMINI_OAUTH_CLIENT_ID"),
            Some("ATMUX_GEMINI_OAUTH_CLIENT_SECRET"),
            |_| None,
        )
        .expect_err("missing variables must fail");
        assert_eq!(missing.kind(), crate::pulse::PulseErrorKind::Configuration);

        let invalid_canary = "invalid\nclient-secret-canary";
        let invalid = GeminiConfig::from_environment_with(
            PathBuf::from("/tmp/oauth-fixture.json"),
            Some("ATMUX_GEMINI_OAUTH_CLIENT_ID"),
            Some("ATMUX_GEMINI_OAUTH_CLIENT_SECRET"),
            |name| {
                if name.ends_with("CLIENT_ID") {
                    Some("fixture-client-id".to_owned())
                } else {
                    Some(invalid_canary.to_owned())
                }
            },
        )
        .expect_err("control characters must fail");
        assert_eq!(invalid.kind(), crate::pulse::PulseErrorKind::Configuration);
        assert!(!invalid.to_string().contains(invalid_canary));
    }

    #[test]
    fn expiry_parser_accepts_cli_milliseconds_seconds_and_iso() {
        assert_eq!(
            parse_expiry_ms(Some(&serde_json::json!(1_786_214_400_123_i64))),
            1_786_214_400_123
        );
        assert_eq!(
            parse_expiry_ms(Some(&serde_json::json!(1_786_214_400_i64))),
            1_786_214_400_000
        );
        assert_eq!(
            parse_expiry_ms(Some(&serde_json::json!("2026-08-08T18:40:00Z"))),
            1_786_214_400_000
        );
    }

    #[test]
    fn quota_rate_limit_preserves_bounded_retry_metadata() {
        let now = Instant::from_epoch_millis(1_786_214_400_000).unwrap();
        let account_id = AccountId::new(1).unwrap();
        let relative = quota_collection(
            account_id,
            now,
            Ok(super::super::HttpResponse {
                status: StatusCode::TOO_MANY_REQUESTS,
                headers: vec![("retry-after".to_owned(), "45".to_owned())],
                body: Vec::new(),
            }),
        );
        assert_eq!(
            relative.outcome,
            CollectionOutcome::RateLimited {
                retry_at: Instant::from_epoch_millis(now.epoch_millis() + 45_000).ok()
            }
        );

        let malformed = quota_collection(
            account_id,
            now,
            Ok(super::super::HttpResponse {
                status: StatusCode::TOO_MANY_REQUESTS,
                headers: vec![("retry-after".to_owned(), "credential-canary".to_owned())],
                body: Vec::new(),
            }),
        );
        assert_eq!(
            malformed.outcome,
            CollectionOutcome::RateLimited { retry_at: None }
        );
        assert!(!format!("{malformed:?}").contains("credential-canary"));
    }
}
