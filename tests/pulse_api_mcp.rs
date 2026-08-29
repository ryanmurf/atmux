#![cfg(feature = "pulse")]

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use atmux::pulse::{
    Account, AccountId, AgentSettings, AlertSubscription, AlertType, CollectionOutcome, Instant,
    Machine, MachineName, Percent, Profile, ProfileName, ProfileOrigin, PulseAccountConfig,
    PulseConfig, PulseProfileConfig, QuotaWindow, QuotaWindowKind, RefreshPolicy, SessionId,
    TokenGrain, TokenSource, UsageSnapshot, Vendor,
    api::{
        AlertSubscriptionInput, MonthlyBudgetPatch, PageRequest, PricingRuleInput,
        ProfileSettingsInput, PulseApi, PulseCapabilities, PulseDeliveryCapabilities,
        PulseMcpMutationRequest, PulseMcpReadRequest, PulseMutationAction, PulseReadResource,
    },
    invalidation::PulseInvalidationHub,
    service::start_embedded,
    store::{AlertEventInput, PricingRule, SqliteStore, Store},
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

struct TestStore {
    directory: PathBuf,
    path: PathBuf,
    store: Arc<SqliteStore>,
}

impl TestStore {
    async fn new() -> Self {
        let directory = private_test_directory(
            "atmux-pulse-api",
            std::process::id(),
            NEXT_DATABASE.fetch_add(1, Ordering::Relaxed),
        );
        let path = directory.join("pulse.sqlite3");
        let store = Arc::new(SqliteStore::open(&path).await.unwrap());
        Self {
            directory,
            path,
            store,
        }
    }

    async fn seed(&self) {
        for value in [1, 2] {
            let account = account(value);
            self.store
                .upsert_account(Account {
                    id: account,
                    identity: format!("operator-{value}@example.test"),
                    display_name: None,
                })
                .await
                .unwrap();
            self.store
                .upsert_machine(Machine {
                    account_id: account,
                    name: machine("midnight"),
                    first_seen: instant(1_000),
                    last_seen: instant(2_000),
                })
                .await
                .unwrap();
            self.store
                .upsert_profile(profile(account, "shared"))
                .await
                .unwrap();
        }
        self.store
            .upsert_profile(profile(account(2), "account-two-only"))
            .await
            .unwrap();
    }

    fn api(&self) -> PulseApi {
        self.api_with_receive(false)
    }

    fn api_with_receive(&self, receive: bool) -> PulseApi {
        PulseApi::new(
            self.store.clone(),
            &[account(1), account(2)],
            PulseCapabilities {
                collect: false,
                serve: true,
                receive,
            },
        )
    }

    fn api_with_health(&self) -> PulseApi {
        self.api().with_management(
            machine("midnight"),
            None,
            PulseDeliveryCapabilities {
                pull: true,
                pane: true,
                channel: false,
            },
        )
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
        let _ = std::fs::remove_dir(&self.directory);
    }
}

fn private_test_directory(prefix: &str, process: u32, nonce: u64) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("{prefix}-{process}-{nonce}"));
    std::fs::create_dir(&directory).expect("create private Pulse test directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mut permissions = std::fs::metadata(&directory)
            .expect("inspect Pulse test directory")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&directory, permissions).expect("protect Pulse test directory");
    }
    directory
}

fn account(value: i64) -> AccountId {
    AccountId::new(value).unwrap()
}

fn instant(value: i64) -> Instant {
    Instant::from_epoch_millis(value).unwrap()
}

fn machine(value: &str) -> MachineName {
    MachineName::new(value).unwrap()
}

fn profile(account_id: AccountId, name: &str) -> Profile {
    Profile {
        account_id,
        name: ProfileName::new(name).unwrap(),
        vendor: Vendor::AnthropicOauth,
        config_dir: Some(PathBuf::from("/secret/profile/home")),
        poll_interval_minutes: 15,
        monthly_budget_usd: None,
        api_key_env: Some("SECRET_CANARY_ENV".to_owned()),
        api_key_file: None,
        refresh: RefreshPolicy::InMemory,
        hidden: false,
        origin: ProfileOrigin::Local,
    }
}

fn page() -> PageRequest {
    PageRequest {
        cursor: None,
        limit: Some(25),
    }
}

fn pricing_rule(key: &str, input: f64) -> PricingRule {
    PricingRule {
        key: key.to_owned(),
        vendor: Vendor::OpenaiCodex,
        model_pattern: "gpt-*".to_owned(),
        settings_match: BTreeMap::new(),
        input_per_million_usd: input,
        output_per_million_usd: 2.0,
        cache_write_5m_per_million_usd: 0.0,
        cache_write_1h_per_million_usd: 0.0,
        cache_read_per_million_usd: 0.1,
    }
}

fn pricing_input(key: &str, input: f64) -> PricingRuleInput {
    let rule = pricing_rule(key, input);
    PricingRuleInput {
        key: rule.key,
        vendor: rule.vendor,
        model_pattern: rule.model_pattern,
        settings_match: rule.settings_match,
        input_per_million_usd: rule.input_per_million_usd,
        output_per_million_usd: rule.output_per_million_usd,
        cache_write_5m_per_million_usd: rule.cache_write_5m_per_million_usd,
        cache_write_1h_per_million_usd: rule.cache_write_1h_per_million_usd,
        cache_read_per_million_usd: rule.cache_read_per_million_usd,
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn account_scoping_blocks_profile_idor_and_redacts_local_references() {
    let test = TestStore::new().await;
    test.seed().await;
    let api = test.api_with_receive(true);

    api.set_visibility(1, "shared".to_owned(), true)
        .await
        .unwrap();
    assert!(
        test.store
            .get_profile(account(1), ProfileName::new("shared").unwrap())
            .await
            .unwrap()
            .unwrap()
            .hidden
    );
    let issued = api
        .mutate_mcp(PulseMcpMutationRequest {
            account_id: 2,
            action: PulseMutationAction::IngestTokenIssue,
            profile: None,
            hidden: None,
            profile_settings: None,
            alert_id: None,
            subscription_id: None,
            message: None,
            subscription: None,
            pricing: None,
            pricing_key: None,
            machine: Some("mcp-reporter".to_owned()),
            token_id: None,
        })
        .await
        .unwrap();
    let token_id = issued["summary"]["id"].as_i64().unwrap();
    assert!(
        issued["token"]
            .as_str()
            .unwrap()
            .starts_with("atmux-pulse-v1.2.")
    );
    let tokens = api
        .read_mcp(PulseMcpReadRequest {
            account_id: 2,
            resource: PulseReadResource::IngestTokens,
            profile: None,
            machine: None,
            since: None,
            through_day: None,
            days: None,
            granularity: None,
            drill: None,
            acknowledged: None,
            alert_id: None,
            cursor: None,
            limit: Some(10),
        })
        .await
        .unwrap();
    let encoded = serde_json::to_string(&tokens).unwrap();
    assert!(!encoded.contains("atmux-pulse-v1"));
    assert!(!encoded.contains("token_hash"));
    let cross_account = api
        .mutate_mcp(PulseMcpMutationRequest {
            account_id: 1,
            action: PulseMutationAction::IngestTokenRevoke,
            profile: None,
            hidden: None,
            profile_settings: None,
            alert_id: None,
            subscription_id: None,
            message: None,
            subscription: None,
            pricing: None,
            pricing_key: None,
            machine: None,
            token_id: Some(token_id),
        })
        .await
        .unwrap_err();
    assert_eq!(cross_account.kind(), atmux::pulse::PulseErrorKind::NotFound);
    api.mutate_mcp(PulseMcpMutationRequest {
        account_id: 2,
        action: PulseMutationAction::IngestTokenRevoke,
        profile: None,
        hidden: None,
        profile_settings: None,
        alert_id: None,
        subscription_id: None,
        message: None,
        subscription: None,
        pricing: None,
        pricing_key: None,
        machine: None,
        token_id: Some(token_id),
    })
    .await
    .unwrap();
    api.upsert_pricing(
        1,
        PricingRuleInput {
            key: "account-one".to_owned(),
            vendor: Vendor::OpenaiCodex,
            model_pattern: "gpt-test".to_owned(),
            settings_match: BTreeMap::default(),
            input_per_million_usd: 1.0,
            output_per_million_usd: 2.0,
            cache_write_5m_per_million_usd: 0.0,
            cache_write_1h_per_million_usd: 0.0,
            cache_read_per_million_usd: 0.1,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        test.store
            .list_pricing_overrides(account(1))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        test.store
            .list_pricing_overrides(account(2))
            .await
            .unwrap()
            .is_empty()
    );
    let pricing = api.pricing(1, page()).await.unwrap();
    assert!(pricing.items.iter().any(|entry| {
        entry.scope == atmux::pulse::api::PricingScope::Override && entry.rule.key == "account-one"
    }));
    assert!(
        !test
            .store
            .get_profile(account(2), ProfileName::new("shared").unwrap())
            .await
            .unwrap()
            .unwrap()
            .hidden
    );
    let error = api
        .set_visibility(1, "account-two-only".to_owned(), true)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), atmux::pulse::PulseErrorKind::NotFound);

    let encoded = serde_json::to_string(&api.profiles(1, page()).await.unwrap()).unwrap();
    assert!(!encoded.contains("/secret/profile/home"), "{encoded}");
    assert!(!encoded.contains("SECRET_CANARY_ENV"), "{encoded}");
    assert!(encoded.contains("credential_source"), "{encoded}");
    assert!(encoded.contains("origin"), "{encoded}");
}

#[tokio::test]
async fn configured_accounts_are_discoverable_without_guessing_ids() {
    let test = TestStore::new().await;
    test.seed().await;
    let api = test.api();

    let accounts = api.accounts().await.unwrap();
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0].id, account(1));
    assert_eq!(accounts[0].identity, "operator-1@example.test");

    let response = atmux::pulse::api::router(api)
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let encoded = response.into_body().collect().await.unwrap().to_bytes();
    let decoded: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded.as_array().unwrap().len(), 2);
    assert_eq!(decoded[1]["identity"], "operator-2@example.test");
    assert!(!String::from_utf8_lossy(&encoded).contains("SECRET_CANARY"));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn discovered_account_serves_nonempty_quota_pace_and_priced_reports_without_cross_account_rows()
 {
    let test = TestStore::new().await;
    let ryan = account(4);
    let other = account(5);
    for (account_id, identity) in [
        (ryan, "ryanmurf@example.test"),
        (other, "other@example.test"),
    ] {
        test.store
            .upsert_account(Account {
                id: account_id,
                identity: identity.to_owned(),
                display_name: (account_id == ryan).then(|| "Ryan".to_owned()),
            })
            .await
            .unwrap();
        for name in ["max", "midnight"] {
            test.store
                .upsert_machine(Machine {
                    account_id,
                    name: machine(name),
                    first_seen: Instant::from_iso8601("2026-08-01T00:00:00Z").unwrap(),
                    last_seen: Instant::from_iso8601("2026-08-09T20:00:00Z").unwrap(),
                })
                .await
                .unwrap();
        }
    }

    let mut claude = profile(ryan, "claude-max");
    claude.vendor = Vendor::AnthropicOauth;
    let mut codex = profile(ryan, "codex-max");
    codex.vendor = Vendor::OpenaiCodex;
    let mut foreign = profile(other, "foreign-profile");
    foreign.vendor = Vendor::AnthropicOauth;
    for configured in [claude.clone(), codex.clone(), foreign.clone()] {
        test.store.upsert_profile(configured).await.unwrap();
    }

    let observed_at = Instant::from_iso8601("2026-08-09T20:00:00Z").unwrap();
    let five_hour_reset = Instant::from_iso8601("2026-08-10T01:00:00Z").unwrap();
    let weekly_reset = Instant::from_iso8601("2026-08-16T00:00:00Z").unwrap();
    for (host, used, polled_at) in [
        (
            "midnight",
            54.0,
            Instant::from_iso8601("2026-08-09T19:55:00Z").unwrap(),
        ),
        ("max", 62.5, observed_at),
    ] {
        test.store
            .append_usage_snapshot(UsageSnapshot {
                account_id: ryan,
                profile: claude.name.clone(),
                machine: machine(host),
                vendor: Vendor::AnthropicOauth,
                windows: vec![
                    QuotaWindow {
                        kind: QuotaWindowKind::FiveHour,
                        used_percent: Percent::new(used).unwrap(),
                        resets_at: five_hour_reset,
                    },
                    QuotaWindow {
                        kind: QuotaWindowKind::RollingSevenDay,
                        used_percent: Percent::new(71.0).unwrap(),
                        resets_at: weekly_reset,
                    },
                ],
                outcome: CollectionOutcome::Success,
                polled_at,
                reporter_version: Some(format!("fixture-{host}")),
            })
            .await
            .unwrap();
    }
    test.store
        .append_usage_snapshot(UsageSnapshot {
            account_id: ryan,
            profile: codex.name.clone(),
            machine: machine("max"),
            vendor: Vendor::OpenaiCodex,
            windows: vec![QuotaWindow {
                kind: QuotaWindowKind::FixedWeekly,
                used_percent: Percent::new(38.0).unwrap(),
                resets_at: weekly_reset,
            }],
            outcome: CollectionOutcome::Success,
            polled_at: observed_at,
            reporter_version: Some("fixture-max".to_owned()),
        })
        .await
        .unwrap();
    test.store
        .append_usage_snapshot(UsageSnapshot {
            account_id: other,
            profile: foreign.name.clone(),
            machine: machine("max"),
            vendor: Vendor::AnthropicOauth,
            windows: vec![QuotaWindow {
                kind: QuotaWindowKind::FiveHour,
                used_percent: Percent::new(99.0).unwrap(),
                resets_at: five_hour_reset,
            }],
            outcome: CollectionOutcome::Success,
            polled_at: observed_at,
            reporter_version: Some("foreign-canary".to_owned()),
        })
        .await
        .unwrap();

    let settings = AgentSettings::default();
    test.store
        .upsert_token_grain(TokenGrain {
            account_id: ryan,
            profile: claude.name.clone(),
            machine: machine("max"),
            session_id: SessionId::new("session-ryan").unwrap(),
            model: "fixture-priced-model".to_owned(),
            settings_hash: settings.sha256().unwrap(),
            settings,
            day: "2026-08-09".to_owned(),
            tokens_in: 1_000_000,
            tokens_out: 500_000,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
            source: TokenSource::Local,
        })
        .await
        .unwrap();
    let foreign_settings = AgentSettings::default();
    test.store
        .upsert_token_grain(TokenGrain {
            account_id: other,
            profile: foreign.name.clone(),
            machine: machine("max"),
            session_id: SessionId::new("session-foreign").unwrap(),
            model: "fixture-priced-model".to_owned(),
            settings_hash: foreign_settings.sha256().unwrap(),
            settings: foreign_settings,
            day: "2026-08-09".to_owned(),
            tokens_in: 9_000_000,
            tokens_out: 9_000_000,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
            source: TokenSource::Local,
        })
        .await
        .unwrap();
    test.store
        .upsert_pricing_default(PricingRule {
            key: "anthropic-priced-fixture".to_owned(),
            vendor: Vendor::AnthropicOauth,
            model_pattern: "fixture-priced-model".to_owned(),
            settings_match: BTreeMap::new(),
            input_per_million_usd: 2.0,
            output_per_million_usd: 4.0,
            cache_write_5m_per_million_usd: 0.0,
            cache_write_1h_per_million_usd: 0.0,
            cache_read_per_million_usd: 0.0,
        })
        .await
        .unwrap();

    let app = atmux::pulse::api::router(PulseApi::new(
        test.store.clone(),
        &[ryan],
        PulseCapabilities {
            collect: false,
            serve: true,
            receive: false,
        },
    ));
    let accounts = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(accounts.status(), StatusCode::OK);
    let accounts: serde_json::Value =
        serde_json::from_slice(&accounts.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(accounts.as_array().unwrap().len(), 1);
    assert_eq!(accounts[0]["id"], 4);
    assert_eq!(accounts[0]["display_name"], "Ryan");

    let usage = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts/4/usage?limit=100")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(usage.status(), StatusCode::OK);
    let usage: serde_json::Value =
        serde_json::from_slice(&usage.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let usage = usage["items"].as_array().unwrap();
    assert_eq!(usage.len(), 3);
    let five_hour = usage
        .iter()
        .find(|row| row["window"]["kind"] == "five_hour")
        .unwrap();
    assert_eq!(five_hour["profile"], "claude-max");
    assert_eq!(five_hour["window"]["used_percent"], 62.5);
    assert_eq!(five_hour["contributors"].as_array().unwrap().len(), 2);
    assert!(
        five_hour["contributors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["machine"] == "max" && row["chosen"] == true)
    );
    assert!(
        usage.iter().any(|row| {
            row["profile"] == "codex-max" && row["window"]["kind"] == "fixed_weekly"
        })
    );
    assert!(!serde_json::to_string(usage).unwrap().contains("foreign"));

    let pace = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts/4/pace?limit=100")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pace.status(), StatusCode::OK);
    let pace: serde_json::Value =
        serde_json::from_slice(&pace.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(pace["items"].as_array().unwrap().len(), 3);
    assert!(pace["items"].as_array().unwrap().iter().any(|row| {
        row["profile"] == "claude-max"
            && row["window"] == "five_hour"
            && row["used_percent"] == 62.5
    }));

    let report = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts/4/reports?through_day=2026-08-09&days=2&granularity=daily&drill=machine")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(report.status(), StatusCode::OK);
    let report: serde_json::Value =
        serde_json::from_slice(&report.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(report["rows_scanned"], 1);
    assert_eq!(report["total"]["total_tokens"], 1_500_000);
    assert_eq!(report["total"]["cost_usd"], 4.0);
    assert_eq!(report["profiles"][0]["profile"], "claude-max");
    assert_eq!(report["profiles"][0]["by_machine"][0]["key"], "max");
    assert_eq!(report["fallback_priced_rows"], 0);

    for uri in [
        "/api/v1/pulse/accounts/5/usage",
        "/api/v1/pulse/accounts/5/reports?through_day=2026-08-09&days=2",
    ] {
        let cross_account = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(cross_account.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn profile_settings_and_health_are_bounded_account_scoped_and_secret_free() {
    let test = TestStore::new().await;
    test.seed().await;
    let api = test.api_with_health();

    api.update_profile_settings(
        1,
        "shared".to_owned(),
        ProfileSettingsInput {
            poll_interval_minutes: Some(30),
            monthly_budget_usd: MonthlyBudgetPatch::Set(250.0),
        },
    )
    .await
    .unwrap();
    let updated = test
        .store
        .get_profile(account(1), ProfileName::new("shared").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.poll_interval_minutes, 30);
    assert_eq!(updated.monthly_budget_usd, Some(250.0));
    assert_eq!(
        updated.config_dir,
        Some(PathBuf::from("/secret/profile/home"))
    );
    assert_eq!(updated.api_key_env.as_deref(), Some("SECRET_CANARY_ENV"));
    let clear = atmux::pulse::api::router(api.clone())
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/pulse/accounts/1/profiles/shared/settings")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"monthly_budget_usd":null}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(clear.status(), StatusCode::NO_CONTENT);
    let cleared = test
        .store
        .get_profile(account(1), ProfileName::new("shared").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cleared.monthly_budget_usd, None);
    assert_eq!(cleared.vendor, Vendor::AnthropicOauth);
    assert_eq!(cleared.origin, ProfileOrigin::Local);
    assert_eq!(
        cleared.config_dir,
        Some(PathBuf::from("/secret/profile/home"))
    );
    assert_eq!(cleared.api_key_env.as_deref(), Some("SECRET_CANARY_ENV"));
    assert_eq!(
        test.store
            .get_profile(account(2), ProfileName::new("shared").unwrap())
            .await
            .unwrap()
            .unwrap()
            .poll_interval_minutes,
        15
    );

    let cross_account = api
        .update_profile_settings(
            1,
            "account-two-only".to_owned(),
            ProfileSettingsInput {
                poll_interval_minutes: Some(30),
                monthly_budget_usd: MonthlyBudgetPatch::Missing,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(cross_account.kind(), atmux::pulse::PulseErrorKind::NotFound);
    let invalid = api
        .update_profile_settings(
            1,
            "shared".to_owned(),
            ProfileSettingsInput {
                poll_interval_minutes: Some(1),
                monthly_budget_usd: MonthlyBudgetPatch::Missing,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(invalid.kind(), atmux::pulse::PulseErrorKind::InvalidInput);

    let health = api.health(1, page()).await.unwrap();
    assert_eq!(health.items.len(), 1);
    assert_eq!(health.items[0].profile.as_str(), "shared");
    assert_eq!(health.items[0].machine.as_str(), "midnight");
    let encoded = serde_json::to_string(&health).unwrap();
    assert!(!encoded.contains("/secret/profile/home"), "{encoded}");
    assert!(!encoded.contains("SECRET_CANARY_ENV"), "{encoded}");

    assert_eq!(
        api.force_poll(1, None).await.unwrap_err().kind(),
        atmux::pulse::PulseErrorKind::Conflict
    );
    assert_eq!(
        api.force_poll(999, None).await.unwrap_err().kind(),
        atmux::pulse::PulseErrorKind::NotFound
    );
    assert_eq!(
        api.force_poll(1, Some("missing".to_owned()))
            .await
            .unwrap_err()
            .kind(),
        atmux::pulse::PulseErrorKind::NotFound
    );
    assert_eq!(
        api.force_poll(1, Some("account-two-only".to_owned()))
            .await
            .unwrap_err()
            .kind(),
        atmux::pulse::PulseErrorKind::NotFound
    );
}

#[tokio::test]
async fn rest_pages_and_errors_are_bounded_and_account_scoped() {
    let test = TestStore::new().await;
    test.seed().await;
    let app = atmux::pulse::api::router(test.api());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts/2/profiles?limit=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["account_id"], 2);
    assert_eq!(body["next_cursor"], "1");

    let invalid = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts/2/profiles?limit=101")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

    let unknown = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts/99/profiles")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let unbounded_report = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts/2/reports?through_day=2026-08-08&days=366")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unbounded_report.status(), StatusCode::BAD_REQUEST);

    let cross_account_force = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/pulse/accounts/1/poll")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"profile":"account-two-only"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_account_force.status(), StatusCode::NOT_FOUND);

    let account_force_without_body = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/pulse/accounts/1/poll")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(account_force_without_body.status(), StatusCode::CONFLICT);

    let credential_injection = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/pulse/accounts/2/profiles/shared/settings")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"poll_interval_minutes":30,"config_dir":"/attacker"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        credential_injection.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn mobile_dashboard_query_pagination_deserializes() {
    let test = TestStore::new().await;
    test.seed().await;
    let app = atmux::pulse::api::router(test.api());

    for uri in [
        "/api/v1/pulse/accounts/2/usage?limit=100",
        "/api/v1/pulse/accounts/2/pace?limit=100",
        "/api/v1/pulse/accounts/2/context?limit=100",
        "/api/v1/pulse/accounts/2/alerts?acknowledged=false&limit=100",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn invalidation_stream_is_bounded_account_scoped_and_reconnect_safe() {
    let test = TestStore::new().await;
    test.seed().await;
    let hub = PulseInvalidationHub::new(&[account(1), account(2)]);
    let api = test.api().with_invalidations(hub.clone());
    let router = atmux::pulse::api::router(api.clone());

    let unknown = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts/99/events")
                .header("last-event-id", "9".repeat(1_000))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    for invalid in ["not-a-revision".to_owned(), "9".repeat(1_000)] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/pulse/accounts/1/events")
                    .header("last-event-id", invalid)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts/1/events")
                .header("last-event-id", "18446744073709551615")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let first = first_body_frame(response).await;
    assert!(first.contains("event: pulse"), "{first}");
    assert!(first.contains("id: 0"), "{first}");
    assert!(first.contains("data: {\"revision\":0}"), "{first}");
    assert!(
        first.len() < 128,
        "initial event must remain bounded: {first}"
    );
    assert!(!first.contains("SECRET_CANARY_ENV"), "{first}");

    let reconnect = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts/1/events")
                .header("last-event-id", "0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let initial = first_body_frame(reconnect).await;
    assert!(initial.contains("id: 0"), "{initial}");

    let mut account_one = hub.subscribe(account(1)).unwrap();
    let mut account_two = hub.subscribe(account(2)).unwrap();
    api.set_visibility(1, "shared".to_owned(), true)
        .await
        .unwrap();
    account_one.receiver.changed().await.unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            account_two.receiver.changed()
        )
        .await
        .is_err(),
        "an account-one mutation must not invalidate account two"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pricing_delete_reverts_to_seeded_default_with_rest_mcp_and_idor_parity() {
    let test = TestStore::new().await;
    test.seed().await;
    test.store
        .upsert_pricing_default(pricing_rule("gpt-seeded", 3.0))
        .await
        .unwrap();
    let hub = PulseInvalidationHub::new(&[account(1), account(2)]);
    let api = test.api().with_invalidations(hub.clone());
    api.upsert_pricing(1, pricing_input("gpt-seeded", 1.0))
        .await
        .unwrap();
    api.upsert_pricing(2, pricing_input("gpt-seeded", 2.0))
        .await
        .unwrap();
    let mut account_one = hub.subscribe(account(1)).unwrap();
    let mut account_two = hub.subscribe(account(2)).unwrap();
    let router = atmux::pulse::api::router(api.clone());

    let deleted = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/pulse/accounts/1/pricing/gpt-seeded")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    account_one.receiver.changed().await.unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            account_two.receiver.changed()
        )
        .await
        .is_err()
    );
    let account_one_pricing = api.pricing(1, page()).await.unwrap();
    assert!(account_one_pricing.items.iter().any(|entry| {
        entry.scope == atmux::pulse::api::PricingScope::Default
            && entry.rule.key == "gpt-seeded"
            && (entry.rule.input_per_million_usd - 3.0).abs() < f64::EPSILON
    }));
    assert!(!account_one_pricing.items.iter().any(|entry| {
        entry.scope == atmux::pulse::api::PricingScope::Override && entry.rule.key == "gpt-seeded"
    }));
    assert_eq!(
        test.store.list_pricing_defaults().await.unwrap().len(),
        1,
        "revert must preserve seeded defaults"
    );

    let cross_account_miss = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/pulse/accounts/1/pricing/gpt-seeded")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let nonexistent_miss = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/v1/pulse/accounts/1/pricing/no-such-rule")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_account_miss.status(), StatusCode::NOT_FOUND);
    assert_eq!(nonexistent_miss.status(), StatusCode::NOT_FOUND);
    let cross_account_body = cross_account_miss
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let nonexistent_body = nonexistent_miss
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(cross_account_body, nonexistent_body);
    assert_eq!(
        test.store
            .list_pricing_overrides(account(2))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        api.delete_pricing_override(1, "../gpt-seeded".to_owned())
            .await
            .unwrap_err()
            .kind(),
        atmux::pulse::PulseErrorKind::InvalidInput
    );

    api.mutate_mcp(PulseMcpMutationRequest {
        account_id: 2,
        action: PulseMutationAction::PricingDelete,
        profile: None,
        hidden: None,
        profile_settings: None,
        alert_id: None,
        subscription_id: None,
        message: None,
        subscription: None,
        pricing: None,
        pricing_key: Some("gpt-seeded".to_owned()),
        machine: None,
        token_id: None,
    })
    .await
    .unwrap();
    account_two.receiver.changed().await.unwrap();
    assert!(
        test.store
            .list_pricing_overrides(account(2))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        api.delete_pricing_override(2, "gpt-seeded".to_owned())
            .await
            .unwrap_err()
            .kind(),
        atmux::pulse::PulseErrorKind::NotFound
    );
}

async fn first_body_frame(response: axum::response::Response) -> String {
    let mut body = response.into_body();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(1), body.frame())
        .await
        .expect("SSE initial event timeout")
        .expect("SSE body ended")
        .expect("SSE body error");
    String::from_utf8(frame.into_data().expect("SSE data frame").to_vec()).unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn receiver_admin_rest_is_fresh_db_safe_redacted_and_account_scoped() {
    let test = TestStore::new().await;
    for value in [1, 2] {
        test.store
            .upsert_account(Account {
                id: account(value),
                identity: format!("receiver-{value}@example.test"),
                display_name: None,
            })
            .await
            .unwrap();
    }
    let app = atmux::pulse::api::router(test.api_with_receive(true));
    let issued = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/pulse/accounts/1/ingest-tokens")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"machine":"remote-max"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(issued.status(), StatusCode::CREATED);
    let issued: serde_json::Value =
        serde_json::from_slice(&issued.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let token = issued["token"].as_str().unwrap().to_owned();
    let token_id = issued["summary"]["id"].as_i64().unwrap();
    assert!(token.starts_with("atmux-pulse-v1.1."));
    assert_eq!(issued["summary"]["machine"], "remote-max");
    assert_eq!(test.store.list_machines(account(1)).await.unwrap().len(), 1);

    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/pulse/accounts/1/ingest-tokens")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_bytes = listed.into_body().collect().await.unwrap().to_bytes();
    let listed_text = String::from_utf8(listed_bytes.to_vec()).unwrap();
    assert!(!listed_text.contains(&token), "plaintext must be one-time");
    assert!(
        !listed_text.contains("token_hash"),
        "hash metadata must stay private"
    );

    let cross_account = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/pulse/accounts/2/ingest-tokens/{token_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_account.status(), StatusCode::NOT_FOUND);
    assert!(
        test.store
            .get_ingest_token(account(1), token_id)
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_none()
    );

    let revoked = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/pulse/accounts/1/ingest-tokens/{token_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
    assert!(
        test.store
            .get_ingest_token(account(1), token_id)
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_some()
    );

    let invalid_machine = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/pulse/accounts/1/ingest-tokens")
                .header("content-type", "application/json")
                .body(Body::from("{\"machine\":\"spoof\\nname\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_machine.status(), StatusCode::BAD_REQUEST);
    assert_eq!(test.store.list_machines(account(1)).await.unwrap().len(), 1);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mcp_contract_requires_explicit_account_and_has_read_mutation_parity() {
    let test = TestStore::new().await;
    test.seed().await;
    let api = test.api();
    let profiles = api
        .read_mcp(PulseMcpReadRequest {
            account_id: 2,
            resource: PulseReadResource::Profiles,
            profile: None,
            machine: None,
            since: None,
            through_day: None,
            days: None,
            granularity: None,
            drill: None,
            acknowledged: None,
            alert_id: None,
            cursor: None,
            limit: Some(10),
        })
        .await
        .unwrap();
    assert!(profiles["items"].as_array().unwrap().iter().all(|profile| {
        profile["account_id"] == 2
            && profile.get("config_dir").is_none()
            && profile.get("api_key_env").is_none()
    }));

    api.mutate_mcp(PulseMcpMutationRequest {
        account_id: 2,
        action: PulseMutationAction::ProfileVisibility,
        profile: Some("shared".to_owned()),
        hidden: Some(true),
        profile_settings: None,
        alert_id: None,
        subscription_id: None,
        message: None,
        subscription: None,
        pricing: None,
        pricing_key: None,
        machine: None,
        token_id: None,
    })
    .await
    .unwrap();
    assert!(
        test.store
            .get_profile(account(2), ProfileName::new("shared").unwrap())
            .await
            .unwrap()
            .unwrap()
            .hidden
    );
    api.mutate_mcp(PulseMcpMutationRequest {
        account_id: 2,
        action: PulseMutationAction::ProfileSettings,
        profile: Some("shared".to_owned()),
        hidden: None,
        profile_settings: Some(ProfileSettingsInput {
            poll_interval_minutes: Some(45),
            monthly_budget_usd: MonthlyBudgetPatch::Set(99.0),
        }),
        alert_id: None,
        subscription_id: None,
        message: None,
        subscription: None,
        pricing: None,
        pricing_key: None,
        machine: None,
        token_id: None,
    })
    .await
    .unwrap();
    api.mutate_mcp(PulseMcpMutationRequest {
        account_id: 2,
        action: PulseMutationAction::ProfileSettings,
        profile: Some("shared".to_owned()),
        hidden: None,
        profile_settings: Some(ProfileSettingsInput {
            poll_interval_minutes: None,
            monthly_budget_usd: MonthlyBudgetPatch::Clear,
        }),
        alert_id: None,
        subscription_id: None,
        message: None,
        subscription: None,
        pricing: None,
        pricing_key: None,
        machine: None,
        token_id: None,
    })
    .await
    .unwrap();
    let mcp_updated = test
        .store
        .get_profile(account(2), ProfileName::new("shared").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mcp_updated.poll_interval_minutes, 45);
    assert_eq!(mcp_updated.monthly_budget_usd, None);
    assert_eq!(
        mcp_updated.api_key_env.as_deref(),
        Some("SECRET_CANARY_ENV")
    );
    let cross_account_force = api
        .mutate_mcp(PulseMcpMutationRequest {
            account_id: 1,
            action: PulseMutationAction::ForcePoll,
            profile: Some("account-two-only".to_owned()),
            hidden: None,
            profile_settings: None,
            alert_id: None,
            subscription_id: None,
            message: None,
            subscription: None,
            pricing: None,
            pricing_key: None,
            machine: None,
            token_id: None,
        })
        .await
        .unwrap_err();
    assert_eq!(
        cross_account_force.kind(),
        atmux::pulse::PulseErrorKind::NotFound
    );
    let unknown = api
        .read_mcp(PulseMcpReadRequest {
            account_id: 999,
            resource: PulseReadResource::Limits,
            profile: None,
            machine: None,
            since: None,
            through_day: None,
            days: None,
            granularity: None,
            drill: None,
            acknowledged: None,
            alert_id: None,
            cursor: None,
            limit: None,
        })
        .await
        .unwrap_err();
    assert_eq!(unknown.kind(), atmux::pulse::PulseErrorKind::NotFound);
}

#[tokio::test]
async fn alert_ack_reply_and_subscriptions_never_cross_accounts() {
    let test = TestStore::new().await;
    test.seed().await;
    let api = test.api();
    let cross_account = api
        .create_subscription(
            1,
            AlertSubscriptionInput {
                profile: "account-two-only".to_owned(),
                alert_type: AlertType::AuthenticationFailure,
                threshold: None,
                cooldown_minutes: 30,
                delivery: None,
                enabled: true,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(cross_account.kind(), atmux::pulse::PulseErrorKind::NotFound);
    let subscription = api
        .create_subscription(
            2,
            AlertSubscriptionInput {
                profile: "shared".to_owned(),
                alert_type: AlertType::AuthenticationFailure,
                threshold: None,
                cooldown_minutes: 30,
                delivery: None,
                enabled: true,
            },
        )
        .await
        .unwrap();
    let event = test
        .store
        .record_alert_if_due(AlertEventInput {
            account_id: account(2),
            subscription_id: subscription.id,
            profile: ProfileName::new("shared").unwrap(),
            alert_type: AlertType::AuthenticationFailure,
            message: "Authentication failed".to_owned(),
            current_value: None,
            threshold: None,
            triggered_at: instant(10_000),
        })
        .await
        .unwrap()
        .unwrap();

    assert!(api.acknowledge_alert(1, event.id).await.is_err());
    assert!(
        api.reply_alert(1, event.id, "handled".to_owned())
            .await
            .is_err()
    );
    let reply = api
        .reply_alert(2, event.id, "handled".to_owned())
        .await
        .unwrap();
    assert_eq!(reply.event_id, event.id);
    assert!(reply.persisted);
    assert!(!serde_json::to_string(&reply).unwrap().contains("handled"));
    let replies = api.alert_replies(2, event.id, page()).await.unwrap();
    assert_eq!(replies.items.len(), 1);
    assert_eq!(replies.items[0].account_id, account(2));
    assert_eq!(replies.items[0].message, "handled");
}

#[tokio::test]
async fn serve_only_runtime_opens_and_bootstraps_one_store_without_a_scheduler() {
    let directory = private_test_directory(
        "atmux-pulse-serve-only",
        std::process::id(),
        NEXT_DATABASE.fetch_add(1, Ordering::Relaxed),
    );
    let path = directory.join("pulse.sqlite3");
    let mut config = PulseConfig {
        serve: true,
        accounts: vec![PulseAccountConfig {
            id: 41,
            identity: "operator@example.test".to_owned(),
            display_name: Some("Operator".to_owned()),
            profiles: vec![PulseProfileConfig {
                name: "codex-max".to_owned(),
                vendor: Vendor::OpenaiCodex,
                config_dir: Some(PathBuf::from("/tmp/codex-max")),
                poll_interval_minutes: 15,
                monthly_budget_usd: None,
                api_key_env: None,
                api_key_file: None,
                refresh: RefreshPolicy::InMemory,
                hidden: false,
            }],
        }],
        ..PulseConfig::default()
    };
    config.database.sqlite_path = Some(path.clone());
    let runtime = start_embedded(&config, "midnight").await.unwrap().unwrap();
    assert_eq!(runtime.accounts().as_ref(), &[account(41)]);
    assert!(runtime.notify_completion().is_err());
    let store = runtime.store();
    assert!(store.get_account(account(41)).await.unwrap().is_some());
    assert!(store.get_account(account(1)).await.unwrap().is_none());
    assert_eq!(
        store.list_machines(account(41)).await.unwrap()[0].name,
        machine("midnight")
    );
    assert_eq!(store.list_profiles(account(41)).await.unwrap().len(), 1);
    runtime.shutdown().await;
    drop(store);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let _ = std::fs::remove_dir(directory);
}

#[test]
fn bootstrap_rejects_inline_identity_guessing_and_unsafe_paths() {
    let mut config = PulseConfig {
        serve: true,
        accounts: vec![PulseAccountConfig {
            id: 1,
            identity: "operator@example.test".to_owned(),
            display_name: None,
            profiles: vec![PulseProfileConfig {
                name: "claude".to_owned(),
                vendor: Vendor::AnthropicOauth,
                config_dir: Some(PathBuf::from("relative/profile")),
                poll_interval_minutes: 15,
                monthly_budget_usd: None,
                api_key_env: None,
                api_key_file: None,
                refresh: RefreshPolicy::InMemory,
                hidden: false,
            }],
        }],
        ..PulseConfig::default()
    };
    assert!(config.validate().is_err());
    config.accounts[0].profiles[0].config_dir = Some(PathBuf::from("/absolute/profile"));
    assert!(config.validate().is_ok());
}

#[test]
fn auth_failure_wire_name_is_preserved() {
    let subscription = AlertSubscription {
        account_id: account(1),
        profile: ProfileName::new("shared").unwrap(),
        alert_type: AlertType::AuthenticationFailure,
        threshold: None,
        cooldown_minutes: 30,
        delivery: None,
        enabled: true,
    };
    let json = serde_json::to_string(&subscription).unwrap();
    assert!(json.contains("auth_failure"));
    let _ = Percent::new(50.0).unwrap();
}
