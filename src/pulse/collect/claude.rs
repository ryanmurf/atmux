//! Anthropic OAuth quota collector state machine.

use std::{fmt, time::Duration};

use serde::Deserialize;

use super::super::{
    credentials::{ClaudeOauthTokens, SecretString},
    error::{PulseError, PulseErrorKind},
    model::{Percent, QuotaWindow, QuotaWindowKind},
    time::Instant,
};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];
const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const INFERENCE_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const INFERENCE_BODY: &[u8] = br#"{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"ok"}]}"#;

/// Fixed request selected by the collector. Secrets are redacted from Debug.
#[derive(Clone, PartialEq)]
pub struct ClaudeRequest {
    pub kind: ClaudeRequestKind,
    pub method: ClaudeHttpMethod,
    pub endpoint: &'static str,
    pub delay: Duration,
    authorization: SecretString,
}

impl ClaudeRequest {
    pub(crate) fn authorization_header(&self) -> String {
        format!("Bearer {}", self.authorization.expose())
    }

    #[must_use]
    pub const fn body(&self) -> &'static [u8] {
        match self.kind {
            ClaudeRequestKind::Usage => &[],
            ClaudeRequestKind::Inference => INFERENCE_BODY,
        }
    }

    #[must_use]
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("Authorization", self.authorization_header()),
            ("anthropic-beta", "oauth-2025-04-20".to_owned()),
        ];
        if self.kind == ClaudeRequestKind::Inference {
            headers.push(("anthropic-version", "2023-06-01".to_owned()));
        }
        headers
    }
}

impl fmt::Debug for ClaudeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeRequest")
            .field("kind", &self.kind)
            .field("method", &self.method)
            .field("endpoint", &self.endpoint)
            .field("delay", &self.delay)
            .field("authorization", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeRequestKind {
    Usage,
    Inference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeHttpMethod {
    Get,
    Post,
}

/// A bounded provider response supplied by the centralized HTTPS transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Source of a normalized Anthropic reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeSource {
    UsageApi,
    InferenceHeaders,
}

/// Identity-free collector result.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeReading {
    pub windows: Vec<QuotaWindow>,
    pub source: ClaudeSource,
}

/// The next action required by the pure collector state machine.
#[derive(Clone, Debug, PartialEq)]
pub enum ClaudeAction {
    Request(ClaudeRequest),
    RefreshRequired,
    Complete(ClaudeReading),
    Failed(PulseError),
}

/// Anthropic collection state. It performs no I/O and never sleeps.
#[derive(Clone, Debug)]
pub struct ClaudeCollector {
    tokens: ClaudeOauthTokens,
    inference_fallback: bool,
    retry_index: usize,
    refresh_attempted: bool,
    pending: Option<ClaudeRequestKind>,
    finished: bool,
}

impl ClaudeCollector {
    #[must_use]
    pub const fn new(tokens: ClaudeOauthTokens, inference_fallback: bool) -> Self {
        Self {
            tokens,
            inference_fallback,
            retry_index: 0,
            refresh_attempted: false,
            pending: None,
            finished: false,
        }
    }

    /// Starts scope-gated collection.
    #[must_use]
    pub fn start(&mut self) -> ClaudeAction {
        if self.finished || self.pending.is_some() {
            return state_error("anthropic collector is not ready to start");
        }
        if self.tokens.has_scope("user:profile") {
            return self.request(ClaudeRequestKind::Usage, Duration::ZERO);
        }
        if self.inference_fallback && self.tokens.has_scope("user:inference") {
            return self.request(ClaudeRequestKind::Inference, Duration::ZERO);
        }
        self.finished = true;
        ClaudeAction::Failed(PulseError::new(
            PulseErrorKind::Authentication,
            "anthropic credential lacks a required usage scope",
        ))
    }

    /// Consumes one response and returns the next action.
    #[must_use]
    pub fn handle_response(&mut self, response: &ClaudeResponse) -> ClaudeAction {
        let Some(kind) = self.pending.take() else {
            return state_error("anthropic collector has no pending request");
        };
        if self.finished {
            return state_error("anthropic collector already finished");
        }
        if response.body.len() > MAX_RESPONSE_BYTES {
            return self.fallback_or_fail(PulseError::new(
                PulseErrorKind::Upstream,
                "anthropic response exceeded its size bound",
            ));
        }
        match kind {
            ClaudeRequestKind::Usage => self.handle_usage(response),
            ClaudeRequestKind::Inference => self.handle_inference(response),
        }
    }

    /// Resumes after the credential layer completed one forced refresh.
    #[must_use]
    pub fn resume_after_refresh(&mut self, refreshed: ClaudeOauthTokens) -> ClaudeAction {
        if self.finished || self.pending.is_some() || !self.refresh_attempted {
            return state_error("anthropic collector is not awaiting refresh");
        }
        self.tokens = refreshed;
        if !self.tokens.has_scope("user:profile") {
            return self.fallback_or_fail(PulseError::new(
                PulseErrorKind::Authentication,
                "refreshed anthropic credential lacks the usage scope",
            ));
        }
        self.request(ClaudeRequestKind::Usage, Duration::ZERO)
    }

    fn handle_usage(&mut self, response: &ClaudeResponse) -> ClaudeAction {
        if (200..300).contains(&response.status) {
            return match parse_usage_response(&response.body) {
                Ok(reading) => self.complete(reading),
                Err(error) => self.fallback_or_fail(error),
            };
        }
        if response.status == 401 && !self.refresh_attempted && self.tokens.can_refresh() {
            self.refresh_attempted = true;
            return ClaudeAction::RefreshRequired;
        }
        if (response.status == 429 || response.status >= 500)
            && let Some(delay) = RETRY_DELAYS.get(self.retry_index).copied()
        {
            self.retry_index += 1;
            return self.request(ClaudeRequestKind::Usage, delay);
        }
        let error = match response.status {
            401 | 403 => PulseError::new(
                PulseErrorKind::Authentication,
                "anthropic usage authentication was rejected",
            ),
            429 => PulseError::new(
                PulseErrorKind::RateLimited,
                "anthropic usage is temporarily throttled",
            ),
            status if status >= 500 => PulseError::new(
                PulseErrorKind::Upstream,
                "anthropic usage service is temporarily unavailable",
            ),
            _ => PulseError::new(
                PulseErrorKind::Upstream,
                "anthropic usage request was rejected",
            ),
        };
        self.fallback_or_fail(error)
    }

    fn handle_inference(&mut self, response: &ClaudeResponse) -> ClaudeAction {
        match parse_inference_headers(&response.headers) {
            Ok(reading) => self.complete(reading),
            Err(error) => {
                self.finished = true;
                ClaudeAction::Failed(if matches!(response.status, 401 | 403) {
                    PulseError::new(
                        PulseErrorKind::Authentication,
                        "anthropic inference authentication was rejected",
                    )
                } else {
                    error
                })
            }
        }
    }

    fn fallback_or_fail(&mut self, error: PulseError) -> ClaudeAction {
        if self.inference_fallback && self.tokens.has_scope("user:inference") {
            self.request(ClaudeRequestKind::Inference, Duration::ZERO)
        } else {
            self.finished = true;
            ClaudeAction::Failed(error)
        }
    }

    fn request(&mut self, kind: ClaudeRequestKind, delay: Duration) -> ClaudeAction {
        self.pending = Some(kind);
        ClaudeAction::Request(ClaudeRequest {
            kind,
            method: if kind == ClaudeRequestKind::Usage {
                ClaudeHttpMethod::Get
            } else {
                ClaudeHttpMethod::Post
            },
            endpoint: if kind == ClaudeRequestKind::Usage {
                USAGE_ENDPOINT
            } else {
                INFERENCE_ENDPOINT
            },
            delay,
            authorization: self.tokens.access_token().clone(),
        })
    }

    fn complete(&mut self, reading: ClaudeReading) -> ClaudeAction {
        self.finished = true;
        ClaudeAction::Complete(reading)
    }
}

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<ApiWindow>,
    seven_day: Option<ApiWindow>,
}

#[derive(Deserialize)]
struct ApiWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

fn parse_usage_response(body: &[u8]) -> Result<ClaudeReading, PulseError> {
    let response: UsageResponse = serde_json::from_slice(body).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Upstream,
            "anthropic usage response was not valid JSON",
        )
    })?;
    let mut windows = Vec::with_capacity(2);
    append_api_window(&mut windows, response.five_hour, QuotaWindowKind::FiveHour)?;
    append_api_window(
        &mut windows,
        response.seven_day,
        QuotaWindowKind::RollingSevenDay,
    )?;
    if windows.is_empty() {
        return Err(PulseError::new(
            PulseErrorKind::Upstream,
            "anthropic usage response contained no complete windows",
        ));
    }
    Ok(ClaudeReading {
        windows,
        source: ClaudeSource::UsageApi,
    })
}

fn append_api_window(
    output: &mut Vec<QuotaWindow>,
    window: Option<ApiWindow>,
    kind: QuotaWindowKind,
) -> Result<(), PulseError> {
    let Some(ApiWindow {
        utilization: Some(utilization),
        resets_at: Some(resets_at),
    }) = window
    else {
        return Ok(());
    };
    let reset = Instant::from_iso8601(&resets_at).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Upstream,
            "anthropic usage response had an invalid reset instant",
        )
    })?;
    output.push(QuotaWindow {
        kind,
        used_percent: Percent::new(utilization).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Upstream,
                "anthropic usage response had an invalid utilization",
            )
        })?,
        resets_at: Instant::from_epoch_millis(reset.epoch_millis().div_euclid(1000) * 1000)
            .map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Upstream,
                    "anthropic usage response had an invalid reset instant",
                )
            })?,
    });
    Ok(())
}

fn parse_inference_headers(headers: &[(String, String)]) -> Result<ClaudeReading, PulseError> {
    let mut windows = Vec::with_capacity(2);
    append_header_window(
        &mut windows,
        headers,
        "anthropic-ratelimit-unified-5h-utilization",
        "anthropic-ratelimit-unified-5h-reset",
        QuotaWindowKind::FiveHour,
    )?;
    append_header_window(
        &mut windows,
        headers,
        "anthropic-ratelimit-unified-7d-utilization",
        "anthropic-ratelimit-unified-7d-reset",
        QuotaWindowKind::RollingSevenDay,
    )?;
    if windows.is_empty() {
        return Err(PulseError::new(
            PulseErrorKind::Upstream,
            "anthropic inference response contained no quota headers",
        ));
    }
    Ok(ClaudeReading {
        windows,
        source: ClaudeSource::InferenceHeaders,
    })
}

fn append_header_window(
    output: &mut Vec<QuotaWindow>,
    headers: &[(String, String)],
    utilization_name: &str,
    reset_name: &str,
    kind: QuotaWindowKind,
) -> Result<(), PulseError> {
    let utilization = header(headers, utilization_name);
    let reset = header(headers, reset_name);
    let (Some(utilization), Some(reset)) = (utilization, reset) else {
        return Ok(());
    };
    let utilization = utilization.parse::<f64>().map_err(|_| {
        PulseError::new(
            PulseErrorKind::Upstream,
            "anthropic quota header had an invalid utilization",
        )
    })? * 100.0;
    let reset = reset.parse::<i64>().map_err(|_| {
        PulseError::new(
            PulseErrorKind::Upstream,
            "anthropic quota header had an invalid reset instant",
        )
    })?;
    output.push(QuotaWindow {
        kind,
        used_percent: Percent::new(utilization).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Upstream,
                "anthropic quota header had an invalid utilization",
            )
        })?,
        resets_at: Instant::from_epoch_millis(reset.saturating_mul(1000)).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Upstream,
                "anthropic quota header had an invalid reset instant",
            )
        })?,
    });
    Ok(())
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn state_error(message: &'static str) -> ClaudeAction {
    ClaudeAction::Failed(PulseError::new(PulseErrorKind::Conflict, message))
}
