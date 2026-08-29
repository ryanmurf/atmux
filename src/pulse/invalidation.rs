//! Bounded, secret-free account invalidations for the Pulse browser UI.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use tokio::sync::watch;

use super::{AccountId, PulseError, PulseErrorKind, PulseResult};

#[derive(Debug)]
struct AccountState {
    revision: u64,
    sender: watch::Sender<u64>,
}

#[derive(Debug)]
struct AccountInvalidations {
    state: Mutex<AccountState>,
}

/// Latest-only invalidation registry bounded by configured Pulse accounts.
#[derive(Clone, Debug)]
pub struct PulseInvalidationHub {
    accounts: Arc<BTreeMap<AccountId, Arc<AccountInvalidations>>>,
}

impl PulseInvalidationHub {
    #[must_use]
    pub fn new(accounts: &[AccountId]) -> Self {
        let accounts = accounts
            .iter()
            .copied()
            .map(|account_id| {
                let (sender, _receiver) = watch::channel(0);
                (
                    account_id,
                    Arc::new(AccountInvalidations {
                        state: Mutex::new(AccountState {
                            revision: 0,
                            sender,
                        }),
                    }),
                )
            })
            .collect();
        Self {
            accounts: Arc::new(accounts),
        }
    }

    /// Publishes one new monotonic revision after a durable mutation commits.
    /// At `u64::MAX` publication stops rather than wrapping or reusing an id.
    #[must_use]
    pub fn publish(&self, account_id: AccountId) -> bool {
        let Some(account) = self.accounts.get(&account_id) else {
            return false;
        };
        advance(account).is_some()
    }

    /// Snapshots the current revision and subscribes to latest-only state.
    ///
    /// # Errors
    ///
    /// Returns not-found for an account outside the configured allowlist.
    /// Subscription is read-only: reconnects never manufacture invalidations
    /// for other clients or consume the monotonic revision space.
    pub fn subscribe(&self, account_id: AccountId) -> PulseResult<PulseInvalidationSubscription> {
        let account = self.accounts.get(&account_id).ok_or_else(|| {
            PulseError::new(PulseErrorKind::NotFound, "Pulse account was not found")
        })?;
        let state = lock_state(account);
        let revision = state.revision;
        let receiver = state.sender.subscribe();
        Ok(PulseInvalidationSubscription { revision, receiver })
    }
}

fn advance(account: &AccountInvalidations) -> Option<u64> {
    let mut state = lock_state(account);
    let revision = state.revision.checked_add(1)?;
    state.revision = revision;
    state.sender.send_replace(revision);
    Some(revision)
}

fn lock_state(account: &AccountInvalidations) -> MutexGuard<'_, AccountState> {
    account
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One account's initial revision and latest-only receiver.
#[derive(Debug)]
pub struct PulseInvalidationSubscription {
    pub revision: u64,
    pub receiver: watch::Receiver<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(value: i64) -> AccountId {
        AccountId::new(value).expect("account")
    }

    #[test]
    fn accounts_are_explicit_and_subscriptions_do_not_publish() {
        let hub = PulseInvalidationHub::new(&[account(1)]);
        let first = hub.subscribe(account(1)).expect("first");
        let second = hub.subscribe(account(1)).expect("second");
        assert_eq!(first.revision, 0);
        assert_eq!(second.revision, 0);
        assert!(!first.receiver.has_changed().expect("open"));
        assert_eq!(
            hub.subscribe(account(2)).expect_err("unknown").kind(),
            PulseErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn slow_subscribers_receive_only_the_latest_monotonic_revision() {
        let hub = PulseInvalidationHub::new(&[account(1), account(2)]);
        let mut subscription = hub.subscribe(account(1)).expect("subscription");
        assert!(hub.publish(account(1)));
        assert!(hub.publish(account(1)));
        assert!(hub.publish(account(2)));
        subscription.receiver.changed().await.expect("changed");
        assert_eq!(*subscription.receiver.borrow_and_update(), 2);
        assert!(!subscription.receiver.has_changed().expect("open"));
    }

    #[test]
    fn exhausted_revision_space_never_wraps_or_reuses_an_id() {
        let id = account(1);
        let hub = PulseInvalidationHub::new(&[id]);
        let account = hub.accounts.get(&id).expect("account");
        lock_state(account).revision = u64::MAX;
        assert!(!hub.publish(id));
        assert_eq!(hub.subscribe(id).expect("snapshot").revision, u64::MAX);
        assert_eq!(lock_state(account).revision, u64::MAX);
    }
}
