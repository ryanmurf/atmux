//! xAI Grok weekly billing with a bounded transcript fallback.
//!
//! This module deliberately contains no process execution. Authentication is
//! never refreshed through a config-directory binary or `PATH` lookup.

use std::{cmp::Reverse, path::Path, time::Duration};

use hyper::{Method, StatusCode, header};
use serde::Deserialize;
use serde_json::Value;

use super::{
    HttpsJsonClient, ScanLimits, SecretRef, read_regular_bounded, scan_regular_files_since,
};
use crate::pulse::{
    AccountId, CollectionOutcome, Instant, MachineName, Percent, ProfileName, PulseError,
    PulseResult, QuotaWindow, QuotaWindowKind, UsageSnapshot, Vendor,
};

const BILLING_ENDPOINT: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const TRANSCRIPT_LOOKBACK_MS: i64 = 24 * 60 * 60 * 1_000;
const MAX_TRANSCRIPT_LINE_BYTES: usize = 64 * 1024;
const TRANSCRIPT_SCAN: ScanLimits = ScanLimits {
    max_depth: 8,
    max_entries: 16_384,
    max_files: 2_048,
    max_file_bytes: 1024 * 1024,
    max_total_bytes: 32 * 1024 * 1024,
    max_duration: Duration::from_secs(2),
};

#[derive(Debug, Deserialize)]
struct BillingEnvelope {
    config: BillingConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingConfig {
    current_period: BillingPeriod,
    credit_usage_percent: f64,
}

#[derive(Debug, Deserialize)]
struct BillingPeriod {
    #[serde(rename = "type")]
    kind: String,
    end: String,
}

/// Parses the live weekly included-credit period.
///
/// # Errors
///
/// Returns an invalid-response error for an oversized/malformed body, a
/// nonweekly period, a nonfinite percentage, or an invalid reset timestamp.
pub fn parse_billing_response(body: &[u8]) -> PulseResult<QuotaWindow> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(PulseError::invalid_input(
            "Grok billing response exceeded its size bound",
        ));
    }
    let envelope: BillingEnvelope = serde_json::from_slice(body)
        .map_err(|_| PulseError::invalid_input("Grok billing response shape was invalid"))?;
    if envelope.config.current_period.kind != "USAGE_PERIOD_TYPE_WEEKLY" {
        return Err(PulseError::invalid_input(
            "Grok billing period was not weekly",
        ));
    }
    if !envelope.config.credit_usage_percent.is_finite() {
        return Err(PulseError::invalid_input(
            "Grok billing percentage was invalid",
        ));
    }
    Ok(QuotaWindow {
        kind: QuotaWindowKind::FixedWeekly,
        used_percent: Percent::new(envelope.config.credit_usage_percent.clamp(0.0, 100.0))?,
        resets_at: Instant::from_iso8601(&envelope.config.current_period.end)?,
    })
}

/// One pure transcript reading ranked by its embedded timestamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrokTranscriptReading {
    pub actual: u64,
    pub limit: u64,
    pub window_hours: u16,
    pub measured_at_ms: i64,
}

/// Parses one bounded JSONL line containing Grok's rolling token-cap signal.
/// Malformed and unrelated lines are ignored without producing an error whose
/// text can be mistaken for provider throttling.
#[must_use]
pub fn parse_transcript_line(line: &str, file_modified_ms: i64) -> Option<GrokTranscriptReading> {
    if line.len() > MAX_TRANSCRIPT_LINE_BYTES {
        return None;
    }
    let marker = "tokens (actual/limit):";
    let suffix = line.split_once(marker)?.1.trim_start();
    let (actual, suffix) = parse_leading_u64(suffix)?;
    let suffix = suffix.trim_start();
    let suffix = suffix.strip_prefix('/')?.trim_start();
    let (limit, _) = parse_leading_u64(suffix)?;
    if limit == 0 {
        return None;
    }
    let window_hours = parse_window_hours(line).unwrap_or(24);
    if !(1..=24 * 31).contains(&window_hours) {
        return None;
    }
    let measured_at_ms = serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|value| embedded_timestamp_ms(&value))
        .unwrap_or(file_modified_ms);
    Some(GrokTranscriptReading {
        actual,
        limit,
        window_hours,
        measured_at_ms,
    })
}

fn parse_leading_u64(value: &str) -> Option<(u64, &str)> {
    let end = value
        .char_indices()
        .take_while(|(_, character)| character.is_ascii_digit())
        .map(|(index, character)| index + character.len_utf8())
        .last()?;
    Some((value[..end].parse().ok()?, &value[end..]))
}

fn parse_window_hours(line: &str) -> Option<u16> {
    let suffix = line.to_ascii_lowercase();
    let suffix = suffix.split_once("rolling ")?.1;
    let (hours, suffix) = parse_leading_u64(suffix)?;
    suffix
        .starts_with("-hour window")
        .then(|| u16::try_from(hours).ok())
        .flatten()
}

fn embedded_timestamp_ms(value: &Value) -> Option<i64> {
    let metadata = value
        .get("params")?
        .get("_meta")
        .and_then(|value| value.get("agentTimestampMs"))
        .and_then(Value::as_i64);
    if metadata.is_some() {
        return metadata;
    }
    let timestamp = value.get("timestamp")?.as_i64()?;
    timestamp.checked_mul(if timestamp < 1_000_000_000_000 {
        1_000
    } else {
        1
    })
}

/// Searches recent regular `updates.jsonl` files under an absolute config
/// directory. Traversal, files, bytes, depth, and wall time are all bounded;
/// symlinks are never followed.
///
/// # Errors
///
/// Returns a bounded unavailable/configuration error when safe discovery is
/// impossible. `Ok(None)` means no fresh usable signal exists.
pub fn collect_transcript_usage(
    config_dir: &Path,
    now: Instant,
) -> PulseResult<Option<QuotaWindow>> {
    let sessions = config_dir.join("sessions");
    let floor = now.epoch_millis().saturating_sub(TRANSCRIPT_LOOKBACK_MS);
    let mut files =
        match scan_regular_files_since(&sessions, TRANSCRIPT_SCAN, Some(floor), |path| {
            path.file_name().and_then(|name| name.to_str()) == Some("updates.jsonl")
        }) {
            Ok(files) => files,
            Err(error) if error.kind() == crate::pulse::PulseErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
    files.sort_by_key(|file| Reverse(file.modified_ms));
    let mut best: Option<GrokTranscriptReading> = None;
    for file in files {
        if best
            .as_ref()
            .is_some_and(|reading| reading.measured_at_ms >= file.modified_ms)
        {
            break;
        }
        let Ok(contents) =
            read_regular_bounded(&file.path, usize::try_from(file.size).unwrap_or(usize::MAX))
        else {
            continue;
        };
        for line in contents.lines().take(16_384) {
            let Some(reading) = parse_transcript_line(line, file.modified_ms) else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|current| reading.measured_at_ms > current.measured_at_ms)
            {
                best = Some(reading);
            }
        }
    }
    let Some(best) = best else {
        return Ok(None);
    };
    if best.measured_at_ms > now.epoch_millis().saturating_add(5 * 60 * 1_000) {
        return Ok(None);
    }
    let reset_ms = best.measured_at_ms.saturating_add(
        i64::from(best.window_hours)
            .saturating_mul(60)
            .saturating_mul(60)
            .saturating_mul(1_000),
    );
    if reset_ms <= now.epoch_millis() {
        return Ok(None);
    }
    let hundredths = u128::from(best.actual)
        .saturating_mul(10_000)
        .checked_div(u128::from(best.limit))
        .unwrap_or(0)
        .min(10_000);
    let used = f64::from(u32::try_from(hundredths).unwrap_or(10_000)) / 100.0;
    Ok(Some(QuotaWindow {
        // The primary Grok allowance is weekly. This fallback is only a
        // conservative observation for the same account-global card.
        kind: QuotaWindowKind::FixedWeekly,
        used_percent: Percent::new(used)?,
        resets_at: Instant::from_epoch_millis(reset_ms / 1_000 * 1_000)?,
    }))
}

/// Direct HTTPS Grok collector. The client version is explicit and validated;
/// no config-directory or `PATH` program is ever launched.
#[derive(Debug)]
pub struct GrokCollector {
    client: HttpsJsonClient,
    client_version: String,
}

impl GrokCollector {
    /// # Errors
    ///
    /// Returns a configuration error for an invalid version or missing TLS
    /// roots.
    pub fn new(client_version: impl Into<String>) -> PulseResult<Self> {
        let client_version = client_version.into();
        if client_version.is_empty()
            || client_version.len() > 32
            || !client_version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            return Err(PulseError::configuration("Grok client version is invalid"));
        }
        Ok(Self {
            client: HttpsJsonClient::new(MAX_RESPONSE_BYTES)?,
            client_version,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn collect(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        machine: MachineName,
        token: &SecretRef,
        transcript_config_dir: Option<&Path>,
        polled_at: Instant,
    ) -> UsageSnapshot {
        let response = match token.resolve() {
            Ok(token) => {
                self.client
                    .request(
                        Method::GET,
                        BILLING_ENDPOINT,
                        &[
                            (
                                header::AUTHORIZATION.as_str(),
                                format!("Bearer {}", token.expose()),
                            ),
                            ("x-grok-client-version", self.client_version.clone()),
                        ],
                        Vec::new(),
                        None,
                    )
                    .await
            }
            Err(error) => Err(error),
        };

        let api_outcome = match &response {
            Ok(response) if response.status == StatusCode::OK => {
                CollectionOutcome::InvalidResponse {
                    code: "grok_billing_response_invalid".to_owned(),
                }
            }
            Ok(response)
                if matches!(
                    response.status,
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ) =>
            {
                CollectionOutcome::AuthenticationFailed {
                    code: "grok_auth_rejected".to_owned(),
                }
            }
            Ok(response) if response.status == StatusCode::TOO_MANY_REQUESTS => {
                CollectionOutcome::RateLimited { retry_at: None }
            }
            Err(error) if error.kind() == crate::pulse::PulseErrorKind::Authentication => {
                CollectionOutcome::AuthenticationFailed {
                    code: "grok_credential_unavailable".to_owned(),
                }
            }
            _ => CollectionOutcome::Unavailable {
                code: "grok_billing_unavailable".to_owned(),
            },
        };
        if let Ok(response) = &response
            && response.status == StatusCode::OK
            && let Ok(window) = parse_billing_response(&response.body)
        {
            return snapshot(
                account_id,
                profile,
                machine,
                vec![window],
                CollectionOutcome::Success,
                polled_at,
            );
        }

        if let Some(config_dir) = transcript_config_dir {
            let config_dir = config_dir.to_path_buf();
            if let Ok(Ok(Some(window))) = tokio::task::spawn_blocking(move || {
                collect_transcript_usage(&config_dir, polled_at)
            })
            .await
            {
                return snapshot(
                    account_id,
                    profile,
                    machine,
                    vec![window],
                    CollectionOutcome::Success,
                    polled_at,
                );
            }
        }
        snapshot(
            account_id,
            profile,
            machine,
            Vec::new(),
            api_outcome,
            polled_at,
        )
    }
}

fn snapshot(
    account_id: AccountId,
    profile: ProfileName,
    machine: MachineName,
    windows: Vec<QuotaWindow>,
    outcome: CollectionOutcome,
    polled_at: Instant,
) -> UsageSnapshot {
    UsageSnapshot {
        account_id,
        profile,
        machine,
        vendor: Vendor::XaiGrok,
        windows,
        outcome,
        polled_at,
        reporter_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
    }
}
