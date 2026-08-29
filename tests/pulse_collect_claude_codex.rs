#![cfg(feature = "pulse")]

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use atmux::pulse::{
    Instant, PulseErrorKind, QuotaWindowKind,
    collect::{
        claude::{ClaudeAction, ClaudeCollector, ClaudeRequestKind, ClaudeResponse, ClaudeSource},
        codex::{
            CodexAction, CodexCollector, CodexLiveResponse, CodexSource, DiscoveryLimits,
            collect_rollout_fallback, parse_live_response,
        },
    },
    credentials::{ClaudeOauthTokens, CodexCredentials},
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "atmux-pulse-collectors-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create temp directory");
        Self(path)
    }

    fn sessions(&self) -> PathBuf {
        let sessions = self.0.join("sessions");
        fs::create_dir_all(&sessions).expect("create sessions");
        sessions
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn now() -> Instant {
    Instant::from_iso8601("2026-08-08T18:40:00Z").expect("now")
}

fn claude_tokens(scopes: &[&str]) -> ClaudeOauthTokens {
    ClaudeOauthTokens::new(
        "anthropic-secret-canary",
        Some("anthropic-refresh-canary".to_owned()),
        now().epoch_millis() + 3_600_000,
        scopes.iter().map(|scope| (*scope).to_owned()).collect(),
    )
    .expect("claude tokens")
}

fn assert_close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < f64::EPSILON);
}

fn response(status: u16) -> ClaudeResponse {
    ClaudeResponse {
        status,
        headers: Vec::new(),
        body: b"provider-secret-body-canary".to_vec(),
    }
}

#[test]
fn anthropic_fixture_parses_and_truncates_resets_to_seconds() {
    let mut collector = ClaudeCollector::new(claude_tokens(&["user:profile"]), false);
    let ClaudeAction::Request(request) = collector.start() else {
        panic!("expected request");
    };
    assert_eq!(request.kind, ClaudeRequestKind::Usage);
    assert_eq!(request.delay, Duration::ZERO);
    assert_eq!(
        request.endpoint,
        "https://api.anthropic.com/api/oauth/usage"
    );
    let debug = format!("{request:?}");
    assert!(!debug.contains("anthropic-secret-canary"));

    let ClaudeAction::Complete(reading) = collector.handle_response(&ClaudeResponse {
        status: 200,
        headers: Vec::new(),
        body: include_bytes!("fixtures/pulse/anthropic-usage.json").to_vec(),
    }) else {
        panic!("expected complete reading");
    };
    assert_eq!(reading.source, ClaudeSource::UsageApi);
    assert_eq!(reading.windows.len(), 2);
    assert_eq!(reading.windows[0].kind, QuotaWindowKind::FiveHour);
    assert_close(reading.windows[0].used_percent.get(), 12.5);
    assert_eq!(reading.windows[0].resets_at.epoch_millis() % 1_000, 0);
    assert_eq!(reading.windows[1].kind, QuotaWindowKind::RollingSevenDay);
}

#[test]
fn anthropic_retries_429_and_5xx_on_the_fixed_schedule() {
    let mut collector = ClaudeCollector::new(claude_tokens(&["user:profile"]), false);
    assert!(matches!(collector.start(), ClaudeAction::Request(_)));
    let ClaudeAction::Request(first_retry) = collector.handle_response(&response(429)) else {
        panic!("expected first retry");
    };
    assert_eq!(first_retry.delay, Duration::from_secs(2));
    let ClaudeAction::Request(second_retry) = collector.handle_response(&response(503)) else {
        panic!("expected second retry");
    };
    assert_eq!(second_retry.delay, Duration::from_secs(5));
    let ClaudeAction::Failed(error) = collector.handle_response(&response(500)) else {
        panic!("expected terminal failure");
    };
    assert_eq!(error.kind(), PulseErrorKind::Upstream);
    assert!(!error.to_string().contains("provider-secret-body-canary"));

    let mut throttled = ClaudeCollector::new(claude_tokens(&["user:profile"]), false);
    let _ = throttled.start();
    let _ = throttled.handle_response(&response(429));
    let _ = throttled.handle_response(&response(429));
    let ClaudeAction::Failed(error) = throttled.handle_response(&response(429)) else {
        panic!("expected throttled failure");
    };
    assert_eq!(error.kind(), PulseErrorKind::RateLimited);
}

#[test]
fn anthropic_refreshes_once_after_unauthorized() {
    let mut collector = ClaudeCollector::new(claude_tokens(&["user:profile"]), false);
    let _ = collector.start();
    assert_eq!(
        collector.handle_response(&response(401)),
        ClaudeAction::RefreshRequired
    );
    let refreshed = ClaudeOauthTokens::new(
        "refreshed-secret",
        Some("rotated-secret".to_owned()),
        now().epoch_millis() + 3_600_000,
        vec!["user:profile".to_owned()],
    )
    .expect("refreshed tokens");
    assert!(matches!(
        collector.resume_after_refresh(refreshed),
        ClaudeAction::Request(_)
    ));
    let ClaudeAction::Failed(error) = collector.handle_response(&response(401)) else {
        panic!("second rejection must be terminal");
    };
    assert_eq!(error.kind(), PulseErrorKind::Authentication);
}

#[test]
fn anthropic_scopes_gate_usage_and_explicit_inference_fallback() {
    let mut missing = ClaudeCollector::new(claude_tokens(&[]), true);
    let ClaudeAction::Failed(error) = missing.start() else {
        panic!("missing scopes must fail");
    };
    assert_eq!(error.kind(), PulseErrorKind::Authentication);

    let mut disabled = ClaudeCollector::new(claude_tokens(&["user:inference"]), false);
    assert!(matches!(disabled.start(), ClaudeAction::Failed(_)));

    let mut enabled = ClaudeCollector::new(claude_tokens(&["user:inference"]), true);
    let ClaudeAction::Request(request) = enabled.start() else {
        panic!("explicit fallback should request inference");
    };
    assert_eq!(request.kind, ClaudeRequestKind::Inference);
    let body: serde_json::Value = serde_json::from_slice(request.body()).expect("request JSON");
    assert_eq!(body["max_tokens"], 1);
    assert_eq!(body["messages"][0]["content"], "ok");
    let ClaudeAction::Complete(reading) = enabled.handle_response(&ClaudeResponse {
        status: 200,
        headers: vec![
            (
                "Anthropic-RateLimit-Unified-5h-Utilization".to_owned(),
                "0.25".to_owned(),
            ),
            (
                "anthropic-ratelimit-unified-5h-reset".to_owned(),
                "1786262400".to_owned(),
            ),
            (
                "anthropic-ratelimit-unified-7d-utilization".to_owned(),
                "0.50".to_owned(),
            ),
            (
                "anthropic-ratelimit-unified-7d-reset".to_owned(),
                "1786838400".to_owned(),
            ),
        ],
        body: b"ignored identity".to_vec(),
    }) else {
        panic!("headers should parse");
    };
    assert_eq!(reading.source, ClaudeSource::InferenceHeaders);
    assert_close(reading.windows[0].used_percent.get(), 25.0);
    assert_close(reading.windows[1].used_percent.get(), 50.0);
}

#[test]
fn codex_request_and_live_fixture_are_identity_free() {
    let credentials =
        CodexCredentials::new("codex-token-canary", "codex-account-canary").expect("credentials");
    let mut collector = CodexCollector::new(credentials);
    let CodexAction::Request(request) = collector.start() else {
        panic!("expected live request");
    };
    assert_eq!(
        request.endpoint,
        "https://chatgpt.com/backend-api/wham/usage"
    );
    let request_debug = format!("{request:?}");
    assert!(!request_debug.contains("canary"));
    let CodexAction::Complete(reading) = collector.handle_live(
        &CodexLiveResponse {
            status: 200,
            body: include_bytes!("fixtures/pulse/codex-wham-usage.json").to_vec(),
        },
        now(),
    ) else {
        panic!("expected live result");
    };
    assert_eq!(reading.source, CodexSource::Live);
    assert_eq!(reading.windows.len(), 1);
    assert_eq!(reading.windows[0].kind, QuotaWindowKind::FixedWeekly);
    assert_close(reading.windows[0].used_percent.get(), 34.5);
    let reading_debug = format!("{reading:?}");
    for identity in ["removed@example.invalid", "removed-user", "removed-account"] {
        assert!(!reading_debug.contains(identity));
    }
}

#[test]
fn codex_duration_classification_supports_dual_swapped_and_weekly_only() {
    let swapped = br#"{
      "rate_limit": {
        "primary_window": {"used_percent": 60, "limit_window_seconds": 604800, "reset_at": 1786838400},
        "secondary_window": {"used_percent": 10, "limit_window_seconds": 18000, "reset_at": 1786262400}
      }
    }"#;
    let reading = parse_live_response(swapped, now()).expect("swapped windows");
    assert_eq!(reading.windows.len(), 2);
    assert_eq!(reading.windows[0].kind, QuotaWindowKind::FiveHour);
    assert_close(reading.windows[0].used_percent.get(), 10.0);
    assert_eq!(reading.windows[1].kind, QuotaWindowKind::FixedWeekly);
    assert_close(reading.windows[1].used_percent.get(), 60.0);

    let weekly_only = include_bytes!("fixtures/pulse/codex-wham-usage.json");
    let reading = parse_live_response(weekly_only, now()).expect("weekly only");
    assert_eq!(reading.windows.len(), 1);
    assert_eq!(reading.windows[0].kind, QuotaWindowKind::FixedWeekly);

    let ambiguous = br#"{"rate_limit":{"primary_window":{"used_percent":1,"reset_at":1786838400},"secondary_window":null}}"#;
    assert!(parse_live_response(ambiguous, now()).is_err());
}

#[test]
fn codex_rollout_ranks_entry_timestamp_and_rejects_future_poisoning() {
    let directory = TempDir::new();
    let sessions = directory.sessions();
    fs::write(
        sessions.join("rollout-fixture.jsonl"),
        include_bytes!("fixtures/pulse/codex-rollout.jsonl"),
    )
    .expect("write fixture");
    fs::write(
        sessions.join("rollout-future.jsonl"),
        r#"{"timestamp":"2099-01-01T00:00:00Z","payload":{"rate_limits":{"primary":{"used_percent":99,"window_minutes":10080,"reset_at":4070908800}}}}"#,
    )
    .expect("write future");
    let reading = collect_rollout_fallback(&directory.0, now(), DiscoveryLimits::default())
        .expect("rollout result");
    assert_eq!(reading.source, CodexSource::Rollout);
    assert_eq!(reading.windows.len(), 1);
    assert_eq!(reading.windows[0].kind, QuotaWindowKind::FixedWeekly);
    assert_close(reading.windows[0].used_percent.get(), 41.0);
}

#[test]
fn codex_expired_and_corrupt_rollouts_use_safe_error_text() {
    let expired = br#"{"rate_limit":{"primary_window":{"used_percent":50,"limit_window_seconds":604800,"reset_at":1},"secondary_window":null}}"#;
    let error = parse_live_response(expired, now()).expect_err("expired");
    assert_safe_staleness_error(&error.to_string());

    let directory = TempDir::new();
    fs::write(
        directory.sessions().join("rollout-corrupt.jsonl"),
        b"{broken\n",
    )
    .expect("write corrupt");
    let error = collect_rollout_fallback(&directory.0, now(), DiscoveryLimits::default())
        .expect_err("corrupt data has no reading");
    assert_safe_staleness_error(&error.to_string());
}

fn assert_safe_staleness_error(message: &str) {
    let lower = message.to_ascii_lowercase();
    assert!(!lower.contains("rate_limit"));
    assert!(!lower.contains("rate-limit"));
    assert!(
        !lower
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|word| word == "429")
    );
}

#[test]
fn codex_rollout_refuses_oversized_files() {
    let directory = TempDir::new();
    fs::write(
        directory.sessions().join("rollout-large.jsonl"),
        vec![b'x'; 257],
    )
    .expect("write oversized");
    let limits = DiscoveryLimits {
        max_file_bytes: 256,
        max_total_bytes: 256,
        ..DiscoveryLimits::default()
    };
    assert!(collect_rollout_fallback(&directory.0, now(), limits).is_err());
}

#[cfg(unix)]
#[test]
fn codex_rollout_does_not_follow_symlink_loops_or_root_links() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new();
    let sessions = directory.sessions();
    fs::write(
        sessions.join("rollout-good.jsonl"),
        include_bytes!("fixtures/pulse/codex-rollout.jsonl"),
    )
    .expect("write fixture");
    symlink(&sessions, sessions.join("loop")).expect("create loop");
    assert!(collect_rollout_fallback(&directory.0, now(), DiscoveryLimits::default()).is_ok());

    let linked = TempDir::new();
    symlink(&sessions, linked.0.join("sessions")).expect("link root");
    assert!(collect_rollout_fallback(&linked.0, now(), DiscoveryLimits::default()).is_err());
}

#[test]
fn codex_rollout_default_enforces_the_ten_thousand_file_cap() {
    let directory = TempDir::new();
    let sessions = directory.sessions();
    for index in 0..=10_000 {
        fs::write(sessions.join(format!("rollout-{index}.jsonl")), b"")
            .expect("create bounded fixture file");
    }
    let limits = DiscoveryLimits {
        max_entries: 20_000,
        max_elapsed: Duration::from_secs(60),
        ..DiscoveryLimits::default()
    };
    assert_eq!(limits.max_files, 10_000);
    assert!(collect_rollout_fallback(&directory.0, now(), limits).is_err());
}
