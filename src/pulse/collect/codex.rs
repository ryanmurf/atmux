//! `OpenAI` Codex live usage and bounded rollout fallback.

use std::{fmt, path::Path, time::Duration};

use serde_json::Value;

use super::{ScanLimits, read_regular_bounded, scan_regular_files};
use crate::pulse::{
    credentials::{CodexCredentials, SecretString},
    error::{PulseError, PulseErrorKind, PulseResult},
    model::{Percent, QuotaWindow, QuotaWindowKind},
    time::Instant,
};

const LIVE_ENDPOINT: &str = "https://chatgpt.com/backend-api/wham/usage";
const MAX_LIVE_BYTES: usize = 1024 * 1024;
const MAX_LINE_BYTES: usize = 512 * 1024;
const FIVE_HOUR_MAX_SECONDS: f64 = 12.0 * 60.0 * 60.0;
const MAX_NESTING_DEPTH: usize = 32;
const MAX_JSON_NODES: usize = 10_000;
const MAX_FUTURE_SKEW_MILLIS: i64 = 5 * 60 * 1000;

/// Fixed live API request. Both credential values are redacted from Debug.
#[derive(Clone, PartialEq)]
pub struct CodexLiveRequest {
    pub endpoint: &'static str,
    access_token: SecretString,
    account_id: SecretString,
}

impl CodexLiveRequest {
    #[must_use]
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "Authorization",
                format!("Bearer {}", self.access_token.expose()),
            ),
            ("ChatGPT-Account-ID", self.account_id.expose().to_owned()),
            ("originator", "Codex Desktop".to_owned()),
        ]
    }
}

impl fmt::Debug for CodexLiveRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexLiveRequest")
            .field("endpoint", &self.endpoint)
            .field("access_token", &"[redacted]")
            .field("account_id", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexLiveResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexSource {
    Live,
    Rollout,
}

/// Identity-free duration-classified usage projection.
#[derive(Clone, Debug, PartialEq)]
pub struct CodexReading {
    pub windows: Vec<QuotaWindow>,
    pub source: CodexSource,
    pub plan_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CodexAction {
    Request(CodexLiveRequest),
    Complete(CodexReading),
    FallbackRequired(PulseError),
    Failed(PulseError),
}

/// One-shot pure live collector. Local fallback is a separate bounded call.
#[derive(Clone, Debug)]
pub struct CodexCollector {
    credentials: CodexCredentials,
    pending: bool,
    finished: bool,
}

impl CodexCollector {
    #[must_use]
    pub const fn new(credentials: CodexCredentials) -> Self {
        Self {
            credentials,
            pending: false,
            finished: false,
        }
    }

    #[must_use]
    pub fn start(&mut self) -> CodexAction {
        if self.pending || self.finished {
            return state_error("codex collector is not ready to start");
        }
        self.pending = true;
        CodexAction::Request(CodexLiveRequest {
            endpoint: LIVE_ENDPOINT,
            access_token: self.credentials.access_token().clone(),
            account_id: self.credentials.account_id().clone(),
        })
    }

    #[must_use]
    pub fn handle_live(&mut self, response: &CodexLiveResponse, now: Instant) -> CodexAction {
        if !self.pending || self.finished {
            return state_error("codex collector has no pending request");
        }
        self.pending = false;
        self.finished = true;
        if response.body.len() > MAX_LIVE_BYTES {
            return CodexAction::FallbackRequired(safe_error(
                PulseErrorKind::Upstream,
                "codex live response exceeded its size bound",
            ));
        }
        if !(200..300).contains(&response.status) {
            return CodexAction::FallbackRequired(match response.status {
                401 | 403 => safe_error(
                    PulseErrorKind::Authentication,
                    "codex live authentication was rejected",
                ),
                status if status >= 500 => safe_error(
                    PulseErrorKind::Upstream,
                    "codex live service is temporarily unavailable",
                ),
                _ => safe_error(PulseErrorKind::Upstream, "codex live request was rejected"),
            });
        }
        match parse_live_response(&response.body, now) {
            Ok(reading) => CodexAction::Complete(reading),
            Err(error) => CodexAction::FallbackRequired(error),
        }
    }
}

/// Bounded local transcript discovery limits.
#[derive(Clone, Copy, Debug)]
pub struct DiscoveryLimits {
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_elapsed: Duration,
    pub max_entry_age: Duration,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self {
            max_depth: 12,
            max_entries: 25_000,
            max_files: 10_000,
            max_file_bytes: 4 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            max_elapsed: Duration::from_millis(500),
            max_entry_age: Duration::from_secs(8 * 24 * 60 * 60),
        }
    }
}

/// Parses a live `/wham/usage` body into typed five-hour/fixed-week fields.
///
/// # Errors
///
/// Returns a secret-free upstream error for malformed, ambiguous, or expired
/// data. Provider identity fields are never retained.
pub fn parse_live_response(body: &[u8], now: Instant) -> PulseResult<CodexReading> {
    if body.len() > MAX_LIVE_BYTES {
        return Err(safe_error(
            PulseErrorKind::Upstream,
            "codex live response exceeded its size bound",
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(|_| {
        safe_error(
            PulseErrorKind::Upstream,
            "codex live response was not valid JSON",
        )
    })?;
    let limits = value
        .get("rate_limit")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            safe_error(
                PulseErrorKind::Upstream,
                "codex live response contained no usage windows",
            )
        })?;
    let primary = limits.get("primary_window").unwrap_or(&Value::Null);
    let secondary = limits.get("secondary_window").unwrap_or(&Value::Null);
    let windows = normalize_windows(primary, secondary, "limit_window_seconds", true, now)?;
    let plan_type = value
        .get("plan_type")
        .and_then(Value::as_str)
        .filter(|value| value.len() <= 128 && !value.chars().any(char::is_control))
        .map(str::to_owned);
    Ok(CodexReading {
        windows,
        source: CodexSource::Live,
        plan_type,
    })
}

/// Discovers and parses the newest current Codex rollout observation.
///
/// # Errors
///
/// Returns a bounded, secret-free error if the validated sessions root has no
/// current classified observation or exceeds a configured safety limit.
pub fn collect_rollout_fallback(
    config_dir: &Path,
    now: Instant,
    limits: DiscoveryLimits,
) -> PulseResult<CodexReading> {
    validate_limits(limits)?;
    let config = canonical_real_directory(config_dir, "codex config directory is unsafe")?;
    let sessions_path = config_dir.join("sessions");
    let sessions = canonical_real_directory(&sessions_path, "codex sessions directory is unsafe")?;
    if !sessions.starts_with(&config) {
        return Err(PulseError::configuration(
            "codex sessions directory escaped its validated root",
        ));
    }
    let files = scan_regular_files(
        &sessions,
        ScanLimits {
            max_depth: limits.max_depth,
            max_entries: limits.max_entries,
            max_files: limits.max_files,
            max_file_bytes: limits.max_file_bytes,
            max_total_bytes: limits.max_total_bytes,
            max_duration: limits.max_elapsed,
        },
        is_rollout,
    )?;
    let max_age_millis = i64::try_from(limits.max_entry_age.as_millis()).unwrap_or(i64::MAX);
    let mut best: Option<RolloutCandidate> = None;
    for file in files {
        let text =
            read_regular_bounded(&file.path, usize::try_from(file.size).unwrap_or(usize::MAX))
                .map_err(|_| {
                    safe_error(
                        PulseErrorKind::Upstream,
                        "codex rollout file could not be read safely",
                    )
                })?;
        if let Some(candidate) = parse_rollout_file(&text, file.modified_ms)? {
            if candidate.timestamp_millis
                > now.epoch_millis().saturating_add(MAX_FUTURE_SKEW_MILLIS)
                || now
                    .epoch_millis()
                    .saturating_sub(candidate.timestamp_millis)
                    > max_age_millis
            {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|current| candidate.timestamp_millis > current.timestamp_millis)
            {
                best = Some(candidate);
            }
        }
    }
    let best = best.ok_or_else(|| {
        safe_error(
            PulseErrorKind::Upstream,
            "codex rollout data has no current usage observation",
        )
    })?;
    let primary = best.limits.get("primary").unwrap_or(&Value::Null);
    let secondary = best.limits.get("secondary").unwrap_or(&Value::Null);
    let windows = normalize_windows(primary, secondary, "window_minutes", false, now)?;
    Ok(CodexReading {
        windows,
        source: CodexSource::Rollout,
        plan_type: None,
    })
}

#[derive(Debug)]
struct RolloutCandidate {
    timestamp_millis: i64,
    limits: serde_json::Map<String, Value>,
}

fn parse_rollout_file(text: &str, modified_millis: i64) -> PulseResult<Option<RolloutCandidate>> {
    let mut best = None;
    for line in text.lines() {
        if line.len() > MAX_LINE_BYTES {
            return Err(safe_error(
                PulseErrorKind::Upstream,
                "codex rollout line exceeded its size bound",
            ));
        }
        if !line.contains("rate_limits") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let timestamp_millis = entry_timestamp(&value).unwrap_or(modified_millis);
        let mut nodes = 0;
        if let Some(limits) = find_limits(&value, 0, &mut nodes)
            && best.as_ref().is_none_or(|candidate: &RolloutCandidate| {
                timestamp_millis >= candidate.timestamp_millis
            })
        {
            best = Some(RolloutCandidate {
                timestamp_millis,
                limits,
            });
        }
    }
    Ok(best)
}

fn find_limits(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
) -> Option<serde_json::Map<String, Value>> {
    if depth > MAX_NESTING_DEPTH || *nodes >= MAX_JSON_NODES {
        return None;
    }
    *nodes += 1;
    match value {
        Value::Object(object) => {
            if let Some(candidate) = object.get("rate_limits").and_then(Value::as_object)
                && candidate.get("primary").is_some_and(Value::is_object)
            {
                return Some(candidate.clone());
            }
            object
                .values()
                .find_map(|child| find_limits(child, depth + 1, nodes))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| find_limits(child, depth + 1, nodes)),
        Value::String(encoded)
            if encoded.len() <= MAX_LINE_BYTES && encoded.contains("rate_limits") =>
        {
            let parsed = serde_json::from_str::<Value>(encoded).ok()?;
            find_limits(&parsed, depth + 1, nodes)
        }
        _ => None,
    }
}

fn entry_timestamp(value: &Value) -> Option<i64> {
    let timestamp = value.get("timestamp")?;
    if let Some(text) = timestamp.as_str() {
        return Instant::from_iso8601(text).ok().map(Instant::epoch_millis);
    }
    let number = value_as_i64(timestamp)?;
    Some(if (-999_999_999_999..=999_999_999_999).contains(&number) {
        number.saturating_mul(1000)
    } else {
        number
    })
}

fn classify_windows<'a>(
    primary: &'a Value,
    secondary: &'a Value,
    duration_field: &str,
    seconds: bool,
) -> PulseResult<Vec<(QuotaWindowKind, &'a Value)>> {
    let primary_duration = duration(primary, duration_field, seconds);
    let secondary_duration = duration(secondary, duration_field, seconds);
    if primary_duration.is_none() && secondary_duration.is_none() {
        if primary.is_object() && secondary.is_object() {
            return Ok(vec![
                (QuotaWindowKind::FiveHour, primary),
                (QuotaWindowKind::FixedWeekly, secondary),
            ]);
        }
        return Err(safe_error(
            PulseErrorKind::Upstream,
            "codex usage response had an ambiguous window duration",
        ));
    }
    let mut classified = Vec::with_capacity(2);
    for (window, window_duration) in [(primary, primary_duration), (secondary, secondary_duration)]
    {
        if let Some(window_duration) = window_duration {
            classified.push((
                if window_duration <= FIVE_HOUR_MAX_SECONDS {
                    QuotaWindowKind::FiveHour
                } else {
                    QuotaWindowKind::FixedWeekly
                },
                window,
            ));
        }
    }
    classified.sort_by_key(|(kind, _)| match kind {
        QuotaWindowKind::FiveHour => 0,
        QuotaWindowKind::FixedWeekly => 1,
        QuotaWindowKind::RollingSevenDay | QuotaWindowKind::MonthlyBudget => 2,
    });
    if classified.is_empty() {
        return Err(safe_error(
            PulseErrorKind::Upstream,
            "codex usage response contained no classified windows",
        ));
    }
    Ok(classified)
}

fn duration(window: &Value, field: &str, seconds: bool) -> Option<f64> {
    let value = window.get(field)?.as_f64()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some(if seconds { value } else { value * 60.0 })
}

fn normalize_windows(
    primary: &Value,
    secondary: &Value,
    duration_field: &str,
    seconds: bool,
    now: Instant,
) -> PulseResult<Vec<QuotaWindow>> {
    let mut windows = Vec::with_capacity(2);
    for (kind, window) in classify_windows(primary, secondary, duration_field, seconds)? {
        if let Some(window) = quota_window(window, kind, now)? {
            windows.push(window);
        }
    }
    if windows.is_empty() {
        return Err(safe_error(
            PulseErrorKind::Upstream,
            "codex usage observation is expired and has no current signal",
        ));
    }
    Ok(windows)
}

fn quota_window(
    window: &Value,
    kind: QuotaWindowKind,
    now: Instant,
) -> PulseResult<Option<QuotaWindow>> {
    let used = window
        .get("used_percent")
        .and_then(Value::as_f64)
        .ok_or_else(|| {
            safe_error(
                PulseErrorKind::Upstream,
                "codex fixed-week window had no utilization",
            )
        })?;
    let reset_seconds = window
        .get("reset_at")
        .and_then(value_as_i64)
        .ok_or_else(|| {
            safe_error(
                PulseErrorKind::Upstream,
                "codex fixed-week window had no reset instant",
            )
        })?;
    let reset_millis = reset_seconds.saturating_mul(1000);
    if reset_millis <= now.epoch_millis() {
        return Ok(None);
    }
    Ok(Some(QuotaWindow {
        kind,
        used_percent: Percent::new(used).map_err(|_| {
            safe_error(
                PulseErrorKind::Upstream,
                "codex fixed-week window had an invalid utilization",
            )
        })?,
        resets_at: Instant::from_epoch_millis(reset_millis).map_err(|_| {
            safe_error(
                PulseErrorKind::Upstream,
                "codex fixed-week window had an invalid reset instant",
            )
        })?,
    }))
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| i64::try_from(value.as_u64()?).ok())
        .or_else(|| value.as_str()?.parse::<i64>().ok())
}

fn canonical_real_directory(path: &Path, message: &'static str) -> PulseResult<std::path::PathBuf> {
    if !path.is_absolute() {
        return Err(PulseError::configuration(message));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| safe_error(PulseErrorKind::NotFound, message))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PulseError::configuration(message));
    }
    path.canonicalize()
        .map_err(|_| safe_error(PulseErrorKind::Upstream, message))
}

fn is_rollout(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("rollout-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        })
}

fn validate_limits(limits: DiscoveryLimits) -> PulseResult<()> {
    if limits.max_depth == 0
        || limits.max_entries == 0
        || limits.max_files == 0
        || limits.max_files > 10_000
        || limits.max_file_bytes == 0
        || limits.max_total_bytes < limits.max_file_bytes
        || limits.max_elapsed.is_zero()
        || limits.max_entry_age.is_zero()
    {
        return Err(PulseError::configuration(
            "codex discovery limits are invalid",
        ));
    }
    Ok(())
}

fn safe_error(kind: PulseErrorKind, message: &'static str) -> PulseError {
    PulseError::new(kind, message)
}

fn state_error(message: &'static str) -> CodexAction {
    CodexAction::Failed(PulseError::new(PulseErrorKind::Conflict, message))
}
