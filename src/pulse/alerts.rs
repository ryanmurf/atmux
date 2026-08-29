//! Account-scoped alert evaluation and durable cooldown handling.
//!
//! Evaluation is deliberately side-effect free. Callers persist candidates
//! through [`record_due_alerts`], whose store implementation enforces each
//! subscription's cooldown transactionally. Notification delivery happens
//! after persistence and is kept outside this module so unavailable channels
//! cannot make a durable alert disappear.
//!
//! Intentional hardening from the frozen TypeScript behavior: authentication
//! alerts require a typed authentication outcome instead of inferred null
//! gauges; cooldowns and threshold shape are checked under a store transaction;
//! pane auth alerts are forbidden; and delivery errors/timeouts expose only a
//! stable error kind after the event is durable.

use std::{future::Future, pin::Pin, time::Duration};

use serde::{Deserialize, Serialize};

use super::{
    error::{PulseError, PulseErrorKind, PulseResult},
    model::{
        AlertDelivery, AlertType, CollectionOutcome, ContextSession, Percent, QuotaWindow,
        QuotaWindowKind, UsageSnapshot,
    },
    store::{AlertEvent, AlertEventInput, Store, StoredAlertSubscription},
    time::Instant,
};

/// Defensive ceiling for one poll evaluation.
pub const MAX_EVALUATED_SUBSCRIPTIONS: usize = 4_096;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// A persisted-event candidate plus safe metadata used by notification sinks.
#[derive(Clone, Debug, PartialEq)]
pub struct AlertCandidate {
    pub event: AlertEventInput,
    pub delivery: Option<AlertDelivery>,
    pub resets_at: Option<Instant>,
}

/// A newly persisted event together with its opt-in delivery destination.
#[derive(Clone, Debug, PartialEq)]
pub struct TriggeredAlert {
    pub event: AlertEvent,
    pub delivery: Option<AlertDelivery>,
    pub resets_at: Option<Instant>,
}

/// Secret-free notification body passed to channel and pane adapters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlertNotification {
    pub event_id: i64,
    pub subscription_id: i64,
    pub account_id: super::model::AccountId,
    pub alert_type: AlertType,
    pub profile: super::model::ProfileName,
    pub message: String,
    pub current_value: Option<Percent>,
    pub threshold: Option<Percent>,
    pub resets_at: Option<Instant>,
}

impl From<&TriggeredAlert> for AlertNotification {
    fn from(triggered: &TriggeredAlert) -> Self {
        Self {
            event_id: triggered.event.id,
            subscription_id: triggered.event.input.subscription_id,
            account_id: triggered.event.input.account_id,
            alert_type: triggered.event.input.alert_type,
            profile: triggered.event.input.profile.clone(),
            message: triggered.event.input.message.clone(),
            current_value: triggered.event.input.current_value,
            threshold: triggered.event.input.threshold,
            resets_at: triggered.resets_at,
        }
    }
}

/// Boxed delivery future used by notification adapters.
pub type AlertDeliveryFuture = Pin<Box<dyn Future<Output = PulseResult<()>> + Send + 'static>>;

/// Capability-gated side-effect adapter. Implementations must never include
/// credential values or raw provider bodies in their errors.
pub trait AlertNotificationSink: Send + Sync {
    /// A Claude client negotiated the experimental channel capability.
    fn channel_available(&self, account_id: super::model::AccountId) -> bool;
    fn notify_channel(&self, notification: AlertNotification) -> AlertDeliveryFuture;
    fn notify_pane(
        &self,
        destination: PaneAlertDestination,
        notification: AlertNotification,
    ) -> AlertDeliveryFuture;
}

/// Account/profile-owned pane route. Notification adapters must validate this
/// tuple against an opt-in route registry before sending terminal input.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PaneAlertDestination {
    pub account_id: super::model::AccountId,
    pub profile: super::model::ProfileName,
    pub pane_id: String,
}

/// Observable result of one fail-soft notification attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AlertDeliveryOutcome {
    NotRequested,
    Delivered,
    ChannelUnavailable,
    Rejected,
    Failed { kind: PulseErrorKind },
}

/// Delivery result linked to its already-durable alert event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertDeliveryResult {
    pub event_id: i64,
    pub outcome: AlertDeliveryOutcome,
}

/// Evaluates usage and authentication subscriptions for one observation.
///
/// The legacy `seven_day_threshold` name covers the provider's long quota
/// window: Anthropic rolling seven-day, Codex/Grok fixed weekly, or `DeepSeek`'s
/// monthly budget. Authentication alerts fire only for a typed authentication
/// failure, never for disabled, rate-limited, unavailable, or malformed data.
///
/// # Errors
///
/// Returns an error for an unbounded subscription set, a cross-account row,
/// or an invalid persisted subscription.
pub fn evaluate_usage_alerts(
    snapshot: &UsageSnapshot,
    subscriptions: &[StoredAlertSubscription],
) -> PulseResult<Vec<AlertCandidate>> {
    snapshot.validate()?;
    validate_subscription_set(snapshot.account_id, subscriptions)?;
    let five_hour = find_window(&snapshot.windows, |kind| kind == QuotaWindowKind::FiveHour);
    let long_window = find_window(&snapshot.windows, |kind| {
        matches!(
            kind,
            QuotaWindowKind::RollingSevenDay
                | QuotaWindowKind::FixedWeekly
                | QuotaWindowKind::MonthlyBudget
        )
    });
    let mut candidates = Vec::new();
    let usage_available = matches!(snapshot.outcome, CollectionOutcome::Success);

    for stored in subscriptions {
        let subscription = &stored.subscription;
        if !subscription.enabled || subscription.profile != snapshot.profile {
            continue;
        }
        let candidate = match subscription.alert_type {
            AlertType::FiveHourThreshold if usage_available => threshold_candidate(
                stored.id,
                snapshot,
                five_hour,
                subscription.threshold,
                subscription.delivery.clone(),
                "5-hour window",
                snapshot.polled_at,
            ),
            AlertType::SevenDayThreshold if usage_available => long_window.and_then(|window| {
                threshold_candidate(
                    stored.id,
                    snapshot,
                    Some(window),
                    subscription.threshold,
                    subscription.delivery.clone(),
                    long_window_label(window.kind),
                    snapshot.polled_at,
                )
            }),
            AlertType::AuthenticationFailure
                if matches!(
                    snapshot.outcome,
                    CollectionOutcome::AuthenticationFailed { .. }
                ) =>
            {
                Some(AlertCandidate {
                    event: AlertEventInput {
                        account_id: snapshot.account_id,
                        subscription_id: stored.id,
                        profile: snapshot.profile.clone(),
                        alert_type: AlertType::AuthenticationFailure,
                        message: format!(
                            "Authentication failure: {} could not authenticate during collection",
                            snapshot.profile.as_str()
                        ),
                        current_value: None,
                        threshold: None,
                        triggered_at: snapshot.polled_at,
                    },
                    delivery: subscription.delivery.clone(),
                    resets_at: None,
                })
            }
            AlertType::FiveHourThreshold
            | AlertType::SevenDayThreshold
            | AlertType::AuthenticationFailure
            | AlertType::ContextThreshold => None,
        };
        if let Some(candidate) = candidate {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

/// Evaluates context subscriptions for one live session observation.
///
/// # Errors
///
/// Returns an error for an unbounded subscription set, a cross-account row,
/// or an invalid persisted subscription.
pub fn evaluate_context_alerts(
    context: &ContextSession,
    subscriptions: &[StoredAlertSubscription],
) -> PulseResult<Vec<AlertCandidate>> {
    context.validate()?;
    validate_subscription_set(context.account_id, subscriptions)?;
    let Some(current) = context.context_percent else {
        return Ok(Vec::new());
    };
    let mut candidates = Vec::new();
    for stored in subscriptions {
        let subscription = &stored.subscription;
        if !subscription.enabled
            || subscription.profile != context.profile
            || subscription.alert_type != AlertType::ContextThreshold
        {
            continue;
        }
        let Some(threshold) = subscription.threshold else {
            continue;
        };
        if current.get() < threshold.get() {
            continue;
        }
        let tokens = context.context_tokens.unwrap_or_default();
        let limit = context.effective_limit.unwrap_or_default();
        candidates.push(AlertCandidate {
            event: AlertEventInput {
                account_id: context.account_id,
                subscription_id: stored.id,
                profile: context.profile.clone(),
                alert_type: AlertType::ContextThreshold,
                message: format!(
                    "Context alert: {} session {} at {:.1}% ({tokens}/{limit} tokens, threshold: {:.1}%). Consider /compact.",
                    context.profile.as_str(),
                    context.session_id.as_str(),
                    current.get(),
                    threshold.get()
                ),
                current_value: Some(current),
                threshold: Some(threshold),
                triggered_at: context.collected_at,
            },
            delivery: subscription.delivery.clone(),
            resets_at: None,
        });
    }
    Ok(candidates)
}

/// Persists candidates sequentially, returning only events whose transactional
/// cooldown check succeeded.
///
/// # Errors
///
/// Returns the first store error. Previously committed events remain durable.
pub async fn record_due_alerts<S: Store + ?Sized>(
    store: &S,
    candidates: Vec<AlertCandidate>,
) -> PulseResult<Vec<TriggeredAlert>> {
    if candidates.len() > MAX_EVALUATED_SUBSCRIPTIONS {
        return Err(PulseError::invalid_input(
            "too many alert candidates in one evaluation",
        ));
    }
    let mut triggered = Vec::new();
    for candidate in candidates {
        if let Some(event) = store.record_alert_if_due(candidate.event).await? {
            triggered.push(TriggeredAlert {
                event,
                delivery: candidate.delivery,
                resets_at: candidate.resets_at,
            });
        }
    }
    Ok(triggered)
}

/// Delivers already-durable alert events without allowing notification failure
/// to undo or hide them. Authentication failures are rejected from panes even
/// if a corrupt store bypassed [`super::model::AlertSubscription::validate`].
pub async fn deliver_triggered_alerts<S: AlertNotificationSink + ?Sized>(
    sink: &S,
    triggered: &[TriggeredAlert],
) -> Vec<AlertDeliveryResult> {
    let mut results = Vec::with_capacity(triggered.len());
    for alert in triggered {
        let outcome = match &alert.delivery {
            None => AlertDeliveryOutcome::NotRequested,
            Some(AlertDelivery::Channel)
                if !sink.channel_available(alert.event.input.account_id) =>
            {
                AlertDeliveryOutcome::ChannelUnavailable
            }
            Some(AlertDelivery::Channel) => {
                bounded_delivery(sink.notify_channel(AlertNotification::from(alert))).await
            }
            Some(AlertDelivery::Pane { .. })
                if alert.event.input.alert_type == AlertType::AuthenticationFailure =>
            {
                AlertDeliveryOutcome::Rejected
            }
            Some(AlertDelivery::Pane { pane_id }) => {
                bounded_delivery(sink.notify_pane(
                    PaneAlertDestination {
                        account_id: alert.event.input.account_id,
                        profile: alert.event.input.profile.clone(),
                        pane_id: pane_id.clone(),
                    },
                    AlertNotification::from(alert),
                ))
                .await
            }
        };
        results.push(AlertDeliveryResult {
            event_id: alert.event.id,
            outcome,
        });
    }
    results
}

fn delivery_outcome(result: PulseResult<()>) -> AlertDeliveryOutcome {
    match result {
        Ok(()) => AlertDeliveryOutcome::Delivered,
        Err(error) => AlertDeliveryOutcome::Failed { kind: error.kind() },
    }
}

async fn bounded_delivery(future: AlertDeliveryFuture) -> AlertDeliveryOutcome {
    match tokio::time::timeout(DELIVERY_TIMEOUT, future).await {
        Ok(result) => delivery_outcome(result),
        Err(_) => AlertDeliveryOutcome::Failed {
            kind: PulseErrorKind::Offline,
        },
    }
}

fn validate_subscription_set(
    account_id: super::model::AccountId,
    subscriptions: &[StoredAlertSubscription],
) -> PulseResult<()> {
    if subscriptions.len() > MAX_EVALUATED_SUBSCRIPTIONS {
        return Err(PulseError::invalid_input(
            "too many alert subscriptions in one evaluation",
        ));
    }
    for stored in subscriptions {
        if stored.subscription.account_id != account_id {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "alert subscription crossed the account boundary",
            ));
        }
        stored.subscription.validate()?;
    }
    Ok(())
}

fn find_window(
    windows: &[QuotaWindow],
    predicate: impl Fn(QuotaWindowKind) -> bool,
) -> Option<&QuotaWindow> {
    windows.iter().find(|window| predicate(window.kind))
}

fn threshold_candidate(
    subscription_id: i64,
    snapshot: &UsageSnapshot,
    window: Option<&QuotaWindow>,
    threshold: Option<Percent>,
    delivery: Option<AlertDelivery>,
    label: &str,
    triggered_at: Instant,
) -> Option<AlertCandidate> {
    let window = window?;
    let threshold = threshold?;
    if window.used_percent.get() < threshold.get() {
        return None;
    }
    let alert_type = if window.kind == QuotaWindowKind::FiveHour {
        AlertType::FiveHourThreshold
    } else {
        AlertType::SevenDayThreshold
    };
    Some(AlertCandidate {
        event: AlertEventInput {
            account_id: snapshot.account_id,
            subscription_id,
            profile: snapshot.profile.clone(),
            alert_type,
            message: format!(
                "Usage alert: {} {label} at {:.1}% (threshold: {:.1}%)",
                snapshot.profile.as_str(),
                window.used_percent.get(),
                threshold.get()
            ),
            current_value: Some(window.used_percent),
            threshold: Some(threshold),
            triggered_at,
        },
        delivery,
        resets_at: Some(window.resets_at),
    })
}

const fn long_window_label(kind: QuotaWindowKind) -> &'static str {
    match kind {
        QuotaWindowKind::RollingSevenDay => "7-day window",
        QuotaWindowKind::FixedWeekly => "weekly window",
        QuotaWindowKind::MonthlyBudget => "monthly budget",
        QuotaWindowKind::FiveHour => "5-hour window",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pulse::model::{
        Account, AccountId, AgentSettings, AlertDelivery, AlertSubscription, Machine, MachineName,
        Profile, ProfileName, RefreshPolicy, SessionId, Vendor,
    };
    use crate::pulse::store::SqliteStore;
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    fn instant(value: i64) -> Instant {
        Instant::from_epoch_millis(value).expect("valid instant")
    }

    fn account(value: i64) -> AccountId {
        AccountId::new(value).expect("valid account")
    }

    fn profile() -> ProfileName {
        ProfileName::new("claude-max").expect("valid profile")
    }

    fn subscription(
        id: i64,
        alert_type: AlertType,
        threshold: Option<f64>,
    ) -> StoredAlertSubscription {
        StoredAlertSubscription {
            id,
            subscription: AlertSubscription {
                account_id: account(1),
                profile: profile(),
                alert_type,
                threshold: threshold.map(|value| Percent::new(value).expect("valid percent")),
                cooldown_minutes: 30,
                delivery: Some(AlertDelivery::Channel),
                enabled: true,
            },
            created_at: instant(1),
        }
    }

    fn triggered(alert_type: AlertType, delivery: Option<AlertDelivery>) -> TriggeredAlert {
        TriggeredAlert {
            event: AlertEvent {
                id: 42,
                input: AlertEventInput {
                    account_id: account(1),
                    subscription_id: 7,
                    profile: profile(),
                    alert_type,
                    message: "safe alert".to_owned(),
                    current_value: None,
                    threshold: None,
                    triggered_at: instant(10_000),
                },
                acknowledged: false,
            },
            delivery,
            resets_at: None,
        }
    }

    struct FakeSink {
        channel_available: bool,
        fail: bool,
        channel_calls: Arc<AtomicU64>,
        pane_calls: Arc<AtomicU64>,
    }

    impl AlertNotificationSink for FakeSink {
        fn channel_available(&self, _account_id: AccountId) -> bool {
            self.channel_available
        }

        fn notify_channel(&self, _notification: AlertNotification) -> AlertDeliveryFuture {
            self.channel_calls.fetch_add(1, Ordering::Relaxed);
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(PulseError::new(
                        PulseErrorKind::Offline,
                        "secret that must not enter the result",
                    ))
                } else {
                    Ok(())
                }
            })
        }

        fn notify_pane(
            &self,
            _destination: PaneAlertDestination,
            _notification: AlertNotification,
        ) -> AlertDeliveryFuture {
            self.pane_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
    }

    fn snapshot(
        vendor: Vendor,
        windows: Vec<(QuotaWindowKind, f64)>,
        outcome: CollectionOutcome,
    ) -> UsageSnapshot {
        UsageSnapshot {
            account_id: account(1),
            profile: profile(),
            machine: MachineName::new("midnight").expect("valid machine"),
            vendor,
            windows: windows
                .into_iter()
                .map(|(kind, used)| QuotaWindow {
                    kind,
                    used_percent: Percent::new(used).expect("valid percent"),
                    resets_at: instant(50_000 + i64::from(kind as u8)),
                })
                .collect(),
            outcome,
            polled_at: instant(10_000),
            reporter_version: None,
        }
    }

    #[test]
    fn usage_thresholds_fire_at_or_above_the_exact_window() {
        let subscriptions = vec![
            subscription(1, AlertType::FiveHourThreshold, Some(90.0)),
            subscription(2, AlertType::SevenDayThreshold, Some(80.0)),
        ];
        let snapshot = snapshot(
            Vendor::AnthropicOauth,
            vec![
                (QuotaWindowKind::FiveHour, 90.0),
                (QuotaWindowKind::RollingSevenDay, 81.5),
            ],
            CollectionOutcome::Success,
        );

        let candidates = evaluate_usage_alerts(&snapshot, &subscriptions).expect("evaluate");

        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0].event.current_value.map(Percent::get),
            Some(90.0)
        );
        assert!(candidates[0].event.message.contains("5-hour window"));
        assert!(candidates[1].event.message.contains("7-day window"));
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.resets_at.is_some())
        );
    }

    #[test]
    fn legacy_seven_day_alert_maps_weekly_and_monthly_provider_windows() {
        let subscriptions = vec![subscription(7, AlertType::SevenDayThreshold, Some(70.0))];
        for (vendor, kind, label) in [
            (
                Vendor::OpenaiCodex,
                QuotaWindowKind::FixedWeekly,
                "weekly window",
            ),
            (
                Vendor::XaiGrok,
                QuotaWindowKind::FixedWeekly,
                "weekly window",
            ),
            (
                Vendor::DeepseekBalance,
                QuotaWindowKind::MonthlyBudget,
                "monthly budget",
            ),
        ] {
            let snapshot = snapshot(vendor, vec![(kind, 75.0)], CollectionOutcome::Success);
            let candidates =
                evaluate_usage_alerts(&snapshot, &subscriptions).expect("evaluate long window");
            assert_eq!(candidates.len(), 1);
            assert!(candidates[0].event.message.contains(label));
        }
    }

    #[test]
    fn auth_alert_requires_a_typed_authentication_failure() {
        let subscriptions = vec![subscription(1, AlertType::AuthenticationFailure, None)];
        for outcome in [
            CollectionOutcome::Disabled {
                code: "disabled".to_owned(),
            },
            CollectionOutcome::RateLimited { retry_at: None },
            CollectionOutcome::Unavailable {
                code: "offline".to_owned(),
            },
            CollectionOutcome::InvalidResponse {
                code: "invalid".to_owned(),
            },
        ] {
            let snapshot = snapshot(Vendor::AnthropicOauth, Vec::new(), outcome);
            assert!(
                evaluate_usage_alerts(&snapshot, &subscriptions)
                    .expect("evaluate non-auth failure")
                    .is_empty()
            );
        }

        let snapshot = snapshot(
            Vendor::AnthropicOauth,
            Vec::new(),
            CollectionOutcome::AuthenticationFailed {
                code: "expired".to_owned(),
            },
        );
        let candidates = evaluate_usage_alerts(&snapshot, &subscriptions).expect("evaluate auth");
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].event.alert_type,
            AlertType::AuthenticationFailure
        );
        assert!(candidates[0].resets_at.is_none());
    }

    #[test]
    fn unavailable_rate_limited_and_disabled_outcomes_never_emit_usage_alerts() {
        let subscriptions = vec![subscription(1, AlertType::FiveHourThreshold, Some(50.0))];
        for outcome in [
            CollectionOutcome::Disabled {
                code: "disabled".to_owned(),
            },
            CollectionOutcome::RateLimited { retry_at: None },
            CollectionOutcome::Unavailable {
                code: "offline".to_owned(),
            },
        ] {
            let snapshot = snapshot(
                Vendor::AnthropicOauth,
                vec![(QuotaWindowKind::FiveHour, 99.0)],
                outcome,
            );
            assert!(
                evaluate_usage_alerts(&snapshot, &subscriptions)
                    .expect("evaluate unavailable usage")
                    .is_empty()
            );
        }
    }

    #[test]
    fn context_alert_carries_session_and_compaction_guidance() {
        let context = ContextSession {
            account_id: account(1),
            profile: profile(),
            machine: MachineName::new("midnight").expect("valid machine"),
            session_id: SessionId::new("session-42").expect("valid session"),
            model: Some("claude-opus-5".to_owned()),
            settings: AgentSettings::default(),
            context_tokens: Some(160_000),
            context_percent: Some(Percent::new(80.0).expect("valid percent")),
            effective_limit: Some(200_000),
            last_active_at: instant(9_000),
            last_reset_at: None,
            collected_at: instant(10_000),
        };
        let subscriptions = vec![subscription(1, AlertType::ContextThreshold, Some(75.0))];

        let candidates = evaluate_context_alerts(&context, &subscriptions).expect("evaluate");

        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].event.message.contains("session-42"));
        assert!(candidates[0].event.message.contains("160000/200000"));
        assert!(candidates[0].event.message.contains("/compact"));
    }

    #[test]
    fn disabled_other_profile_and_below_threshold_subscriptions_do_not_fire() {
        let mut disabled = subscription(1, AlertType::FiveHourThreshold, Some(50.0));
        disabled.subscription.enabled = false;
        let mut other = subscription(2, AlertType::FiveHourThreshold, Some(50.0));
        other.subscription.profile = ProfileName::new("other").expect("valid profile");
        let below = subscription(3, AlertType::FiveHourThreshold, Some(95.0));
        let snapshot = snapshot(
            Vendor::AnthropicOauth,
            vec![(QuotaWindowKind::FiveHour, 90.0)],
            CollectionOutcome::Success,
        );

        assert!(
            evaluate_usage_alerts(&snapshot, &[disabled, other, below])
                .expect("evaluate")
                .is_empty()
        );
    }

    #[test]
    fn cross_account_and_unbounded_sets_fail_closed() {
        let snapshot = snapshot(
            Vendor::AnthropicOauth,
            vec![(QuotaWindowKind::FiveHour, 90.0)],
            CollectionOutcome::Success,
        );
        let mut other_account = subscription(1, AlertType::FiveHourThreshold, Some(50.0));
        other_account.subscription.account_id = account(2);
        assert!(evaluate_usage_alerts(&snapshot, &[other_account]).is_err());

        let repeated = subscription(1, AlertType::FiveHourThreshold, Some(50.0));
        let too_many = vec![repeated; MAX_EVALUATED_SUBSCRIPTIONS + 1];
        assert!(evaluate_usage_alerts(&snapshot, &too_many).is_err());
    }

    #[tokio::test]
    async fn evaluated_alerts_use_the_store_transactional_cooldown() {
        let database_id = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "atmux-pulse-alert-engine-{}-{database_id}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).expect("private alert test directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .expect("secure alert test directory");
        }
        let path = directory.join("pulse.sqlite3");
        let store = SqliteStore::open(&path).await.expect("open store");
        store
            .upsert_account(Account {
                id: account(1),
                identity: "ryan@example.test".to_owned(),
                display_name: None,
            })
            .await
            .expect("account");
        store
            .upsert_machine(Machine {
                account_id: account(1),
                name: MachineName::new("midnight").expect("machine"),
                first_seen: instant(1),
                last_seen: instant(1),
            })
            .await
            .expect("machine");
        store
            .upsert_profile(Profile {
                account_id: account(1),
                name: profile(),
                vendor: Vendor::AnthropicOauth,
                config_dir: Some(PathBuf::from("/tmp/claude-max")),
                poll_interval_minutes: 15,
                monthly_budget_usd: None,
                api_key_env: None,
                api_key_file: None,
                refresh: RefreshPolicy::InMemory,
                hidden: false,
                origin: crate::pulse::ProfileOrigin::Local,
            })
            .await
            .expect("profile");
        let stored = store
            .create_alert_subscription(
                subscription(0, AlertType::FiveHourThreshold, Some(80.0)).subscription,
                instant(1),
            )
            .await
            .expect("subscription");
        let first_snapshot = snapshot(
            Vendor::AnthropicOauth,
            vec![(QuotaWindowKind::FiveHour, 90.0)],
            CollectionOutcome::Success,
        );
        let first = evaluate_usage_alerts(&first_snapshot, std::slice::from_ref(&stored))
            .expect("evaluate first");
        assert_eq!(
            record_due_alerts(&store, first)
                .await
                .expect("record first")
                .len(),
            1
        );

        let same_poll = evaluate_usage_alerts(&first_snapshot, std::slice::from_ref(&stored))
            .expect("evaluate duplicate");
        assert!(
            record_due_alerts(&store, same_poll)
                .await
                .expect("cooldown")
                .is_empty()
        );

        drop(store);
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = std::fs::remove_file(candidate);
        }
        std::fs::remove_dir(directory).expect("remove alert test directory");
    }

    #[tokio::test]
    async fn delivery_is_capability_gated_fail_soft_and_never_injects_auth_failures() {
        let channel_calls = Arc::new(AtomicU64::new(0));
        let pane_calls = Arc::new(AtomicU64::new(0));
        let unavailable = FakeSink {
            channel_available: false,
            fail: false,
            channel_calls: Arc::clone(&channel_calls),
            pane_calls: Arc::clone(&pane_calls),
        };
        let results = deliver_triggered_alerts(
            &unavailable,
            &[
                triggered(AlertType::FiveHourThreshold, Some(AlertDelivery::Channel)),
                triggered(
                    AlertType::AuthenticationFailure,
                    Some(AlertDelivery::Pane {
                        pane_id: "%7".to_owned(),
                    }),
                ),
                triggered(AlertType::SevenDayThreshold, None),
            ],
        )
        .await;
        assert_eq!(
            results
                .iter()
                .map(|result| result.outcome.clone())
                .collect::<Vec<_>>(),
            vec![
                AlertDeliveryOutcome::ChannelUnavailable,
                AlertDeliveryOutcome::Rejected,
                AlertDeliveryOutcome::NotRequested,
            ]
        );
        assert_eq!(channel_calls.load(Ordering::Relaxed), 0);
        assert_eq!(pane_calls.load(Ordering::Relaxed), 0);

        let failing = FakeSink {
            channel_available: true,
            fail: true,
            channel_calls: Arc::clone(&channel_calls),
            pane_calls,
        };
        let failed = deliver_triggered_alerts(
            &failing,
            &[triggered(
                AlertType::FiveHourThreshold,
                Some(AlertDelivery::Channel),
            )],
        )
        .await;
        assert_eq!(
            failed[0].outcome,
            AlertDeliveryOutcome::Failed {
                kind: PulseErrorKind::Offline,
            }
        );
        let json = serde_json::to_string(&failed).expect("serialize delivery result");
        assert!(!json.contains("secret"));
        assert_eq!(channel_calls.load(Ordering::Relaxed), 1);
    }
}
