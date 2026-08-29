//! Capability-gated alert delivery adapters.

use std::sync::Arc;

use super::{
    AccountId, AlertDelivery, PulseError, PulseErrorKind,
    alerts::{AlertDeliveryFuture, AlertNotification, AlertNotificationSink, PaneAlertDestination},
    reset::{ResetDeliveryFuture, ResetNotification, ResetNotificationSink},
    store::Store,
};
use crate::control::{ControlPlane, ErrorKind, error_kind};

/// Live Claude-channel boundary. Implementations return true only while a
/// client for that account has actually negotiated the channel capability.
pub trait NegotiatedAlertChannel: Send + Sync {
    fn available(&self, account_id: AccountId) -> bool;
    fn notify(&self, notification: AlertNotification) -> AlertDeliveryFuture;
    fn notify_reset(&self, notification: ResetNotification) -> ResetDeliveryFuture;
}

/// One injectable runtime capability covering alert and reset delivery.
pub trait PulseNotificationSink: AlertNotificationSink + ResetNotificationSink {}

impl<T> PulseNotificationSink for T where T: AlertNotificationSink + ResetNotificationSink {}

/// Safe default for today's stateless MCP transport.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableAlertSink;

impl AlertNotificationSink for UnavailableAlertSink {
    fn channel_available(&self, _account_id: AccountId) -> bool {
        false
    }

    fn notify_channel(&self, _notification: AlertNotification) -> AlertDeliveryFuture {
        Box::pin(async {
            Err(PulseError::new(
                PulseErrorKind::Offline,
                "no negotiated alert channel is available",
            ))
        })
    }

    fn notify_pane(
        &self,
        _destination: PaneAlertDestination,
        _notification: AlertNotification,
    ) -> AlertDeliveryFuture {
        Box::pin(async {
            Err(PulseError::new(
                PulseErrorKind::Offline,
                "no authorized alert pane route is available",
            ))
        })
    }
}

impl ResetNotificationSink for UnavailableAlertSink {
    fn channel_available(&self, _account_id: AccountId) -> bool {
        false
    }

    fn notify_reset(&self, _notification: ResetNotification) -> ResetDeliveryFuture {
        Box::pin(async {
            Err(PulseError::new(
                PulseErrorKind::Offline,
                "no negotiated reset channel is available",
            ))
        })
    }
}

/// Control-plane delivery with an account-scoped subscription lookup.
///
/// A subscription's caller-controlled pane id is never sent directly. The
/// complete destination must still match the durable subscription that caused
/// this event, then resolve through the pane's owning machine.
#[derive(Clone)]
pub struct ControlPlaneAlertSink {
    control: ControlPlane,
    store: Arc<dyn Store>,
    channel: Option<Arc<dyn NegotiatedAlertChannel>>,
}

impl ControlPlaneAlertSink {
    #[must_use]
    pub fn new(
        control: ControlPlane,
        store: Arc<dyn Store>,
        channel: Option<Arc<dyn NegotiatedAlertChannel>>,
    ) -> Self {
        Self {
            control,
            store,
            channel,
        }
    }
}

impl AlertNotificationSink for ControlPlaneAlertSink {
    fn channel_available(&self, account_id: AccountId) -> bool {
        self.channel
            .as_ref()
            .is_some_and(|channel| channel.available(account_id))
    }

    fn notify_channel(&self, notification: AlertNotification) -> AlertDeliveryFuture {
        let Some(channel) = self.channel.clone() else {
            return Box::pin(async {
                Err(PulseError::new(
                    PulseErrorKind::Offline,
                    "no negotiated alert channel is available",
                ))
            });
        };
        if !channel.available(notification.account_id) {
            return Box::pin(async {
                Err(PulseError::new(
                    PulseErrorKind::Offline,
                    "the negotiated alert channel is no longer available",
                ))
            });
        }
        channel.notify(notification)
    }

    fn notify_pane(
        &self,
        destination: PaneAlertDestination,
        notification: AlertNotification,
    ) -> AlertDeliveryFuture {
        if notification.account_id != destination.account_id
            || notification.profile != destination.profile
        {
            return Box::pin(async {
                Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "alert pane route is not owned by this account and profile",
                ))
            });
        }
        let control = self.control.clone();
        let store = Arc::clone(&self.store);
        let text = pane_message(&notification);
        Box::pin(async move {
            let subscriptions = store
                .list_alert_subscriptions(destination.account_id)
                .await
                .map_err(|error| {
                    PulseError::new(error.kind(), "alert pane ownership lookup failed")
                })?;
            let authorized = subscriptions.iter().any(|stored| {
                stored.id == notification.subscription_id
                    && stored.subscription.profile == destination.profile
                    && matches!(
                        &stored.subscription.delivery,
                        Some(AlertDelivery::Pane { pane_id }) if pane_id == &destination.pane_id
                    )
            });
            if !authorized {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "alert pane route is not owned by this account and profile",
                ));
            }
            control
                .send_text(&destination.pane_id, text, true)
                .await
                .map_err(|error| {
                    let (kind, message) = match error_kind(&error) {
                        ErrorKind::NotFound => (PulseErrorKind::NotFound, "alert pane is gone"),
                        ErrorKind::Offline => {
                            (PulseErrorKind::Offline, "alert pane owner is offline")
                        }
                        ErrorKind::BadRequest | ErrorKind::Conflict => {
                            (PulseErrorKind::Conflict, "alert pane route is invalid")
                        }
                        ErrorKind::Upstream => (
                            PulseErrorKind::Upstream,
                            "alert pane owner rejected delivery",
                        ),
                        ErrorKind::Internal => {
                            (PulseErrorKind::Internal, "alert pane delivery failed")
                        }
                    };
                    PulseError::new(kind, message)
                })
        })
    }
}

impl ResetNotificationSink for ControlPlaneAlertSink {
    fn channel_available(&self, account_id: AccountId) -> bool {
        self.channel
            .as_ref()
            .is_some_and(|channel| channel.available(account_id))
    }

    fn notify_reset(&self, notification: ResetNotification) -> ResetDeliveryFuture {
        let Some(channel) = self.channel.clone() else {
            return Box::pin(async {
                Err(PulseError::new(
                    PulseErrorKind::Offline,
                    "no negotiated reset channel is available",
                ))
            });
        };
        if !channel.available(notification.account_id) {
            return Box::pin(async {
                Err(PulseError::new(
                    PulseErrorKind::Offline,
                    "the negotiated reset channel is no longer available",
                ))
            });
        }
        channel.notify_reset(notification)
    }
}

const fn alert_label(alert_type: super::AlertType) -> &'static str {
    match alert_type {
        super::AlertType::FiveHourThreshold => "5-hour usage",
        super::AlertType::SevenDayThreshold => "long-window usage",
        super::AlertType::AuthenticationFailure => "authentication",
        super::AlertType::ContextThreshold => "context",
    }
}

fn pane_message(notification: &AlertNotification) -> String {
    format!(
        "[Pulse {} alert #{}] {}",
        alert_label(notification.alert_type),
        notification.event_id,
        notification.message
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pulse::{
        Account, AlertSubscription, AlertType, Instant, Percent, Profile, ProfileName,
        ProfileOrigin, RefreshPolicy, Vendor,
    };
    use std::path::PathBuf;

    fn notification() -> AlertNotification {
        AlertNotification {
            event_id: 42,
            subscription_id: 7,
            account_id: AccountId::new(1).expect("account"),
            alert_type: AlertType::ContextThreshold,
            profile: ProfileName::new("claude").expect("profile"),
            message: "session is at 90%; consider /compact".to_owned(),
            current_value: None,
            threshold: None,
            resets_at: Some(Instant::from_epoch_millis(1_000).expect("instant")),
        }
    }

    #[test]
    fn pane_serialization_is_exact_and_secret_free_by_construction() {
        assert_eq!(
            pane_message(&notification()),
            "[Pulse context alert #42] session is at 90%; consider /compact"
        );
    }

    #[tokio::test]
    async fn default_sink_never_claims_a_negotiated_channel() {
        let sink = UnavailableAlertSink;
        let notification = notification();
        assert!(!AlertNotificationSink::channel_available(
            &sink,
            notification.account_id
        ));
        assert_eq!(
            sink.notify_channel(notification)
                .await
                .expect_err("unavailable")
                .kind(),
            PulseErrorKind::Offline
        );
    }

    #[tokio::test]
    async fn pane_ids_are_rejected_without_an_owned_route() {
        let store = Arc::new(
            crate::pulse::store::SqliteStore::open(":memory:")
                .await
                .expect("store"),
        );
        let sink = ControlPlaneAlertSink::new(
            crate::control::test_control(&[]),
            store as Arc<dyn Store>,
            None,
        );
        let notification = notification();
        let destination = PaneAlertDestination {
            account_id: notification.account_id,
            profile: notification.profile.clone(),
            pane_id: "%999".to_owned(),
        };
        assert_eq!(
            sink.notify_pane(destination, notification)
                .await
                .expect_err("unowned pane")
                .kind(),
            PulseErrorKind::Conflict
        );
    }

    #[tokio::test]
    async fn owned_remote_pane_routes_fail_soft_while_offline() {
        let store = Arc::new(
            crate::pulse::store::SqliteStore::open(":memory:")
                .await
                .expect("store"),
        );
        let account_id = AccountId::new(1).expect("account");
        let profile = ProfileName::new("claude").expect("profile");
        store
            .upsert_account(Account {
                id: account_id,
                identity: "pane@example.test".to_owned(),
                display_name: None,
            })
            .await
            .expect("account");
        store
            .upsert_profile(Profile {
                account_id,
                name: profile.clone(),
                vendor: Vendor::AnthropicOauth,
                config_dir: Some(PathBuf::from("/tmp/claude")),
                poll_interval_minutes: 15,
                monthly_budget_usd: None,
                api_key_env: None,
                api_key_file: None,
                refresh: RefreshPolicy::Never,
                hidden: false,
                origin: ProfileOrigin::Local,
            })
            .await
            .expect("profile");
        let pane_id = "midnight~%9".to_owned();
        let subscription = store
            .create_alert_subscription(
                AlertSubscription {
                    account_id,
                    profile: profile.clone(),
                    alert_type: AlertType::ContextThreshold,
                    threshold: Some(Percent::new(90.0).expect("threshold")),
                    cooldown_minutes: 30,
                    delivery: Some(AlertDelivery::Pane {
                        pane_id: pane_id.clone(),
                    }),
                    enabled: true,
                },
                Instant::from_epoch_millis(1_000).expect("instant"),
            )
            .await
            .expect("subscription");
        let sink = ControlPlaneAlertSink::new(
            crate::control::test_control(&["midnight"]),
            Arc::clone(&store) as Arc<dyn Store>,
            None,
        );
        let mut notification = notification();
        notification.subscription_id = subscription.id;
        notification.account_id = account_id;
        notification.profile = profile.clone();
        let destination = PaneAlertDestination {
            account_id,
            profile,
            pane_id,
        };
        assert_eq!(
            sink.notify_pane(destination, notification)
                .await
                .expect_err("offline owner")
                .kind(),
            PulseErrorKind::Offline
        );
    }
}
