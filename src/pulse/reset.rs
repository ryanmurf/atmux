//! Durable rate-limit reset scheduling and fail-soft channel delivery.
//!
//! Unlike the frozen in-memory timers, jobs survive restart, deduplicate by
//! account/profile/reset, use expiring delivery leases, and remain pending when
//! a negotiated channel is unavailable or times out.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::{sync::Notify, task::JoinHandle};

use super::{
    AccountId, CollectionOutcome, Instant, ProfileName, PulseError, PulseErrorKind, PulseResult,
    UsageSnapshot,
    scheduler::SchedulerClock,
    store::{ResetResumeInput, ResetResumeJob, ResetResumeLimits, Store},
};

const DELIVERY_LEASE_MILLIS: i64 = 5 * 60 * 1_000;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_RECHECK_MILLIS: u64 = 60_000;
const MAX_SCHEDULER_ACCOUNTS: usize = 1_024;
const RESTORE_LIMIT: usize = 4_096;

/// Secret-free reset metadata sent only to a negotiated Claude channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetNotification {
    pub account_id: AccountId,
    pub profile: ProfileName,
    pub job_id: i64,
    pub resets_at: Instant,
    pub resume_at: Instant,
}

pub type ResetDeliveryFuture = Pin<Box<dyn Future<Output = PulseResult<()>> + Send + 'static>>;

/// Live capability boundary for reset notifications.
pub trait ResetNotificationSink: Send + Sync {
    fn channel_available(&self, account_id: AccountId) -> bool;
    fn notify_reset(&self, notification: ResetNotification) -> ResetDeliveryFuture;
}

/// Finds the earliest provider retry/reset strictly after `now`, but only for
/// a typed rate-limit observation. A production `Retry-After` value remains
/// useful even when the rejected response carried no quota windows.
#[must_use]
pub fn earliest_future_reset(snapshot: &UsageSnapshot, now: Instant) -> Option<Instant> {
    let CollectionOutcome::RateLimited { retry_at } = &snapshot.outcome else {
        return None;
    };
    let window_reset = snapshot
        .windows
        .iter()
        .map(|window| window.resets_at)
        .filter(|reset| *reset > now)
        .min();
    retry_at
        .as_ref()
        .copied()
        .filter(|retry| *retry > now)
        .into_iter()
        .chain(window_reset)
        .min()
}

/// Persists one deduplicated resume job for a rate-limited snapshot.
///
/// # Errors
///
/// Returns validation or store errors. Non-rate-limited observations and
/// rate-limit observations without a future reset return `Ok(None)`.
pub async fn schedule_rate_limit_resume<S: Store + ?Sized>(
    store: &S,
    snapshot: &UsageSnapshot,
    now: Instant,
    limits: ResetResumeLimits,
) -> PulseResult<Option<ResetResumeJob>> {
    snapshot.validate()?;
    let Some(resets_at) = earliest_future_reset(snapshot, now) else {
        return Ok(None);
    };
    store
        .schedule_reset_resume(
            ResetResumeInput {
                account_id: snapshot.account_id,
                profile: snapshot.profile.clone(),
                resets_at,
                scheduled_at: now,
            },
            limits,
        )
        .await
        .map(Some)
}

/// Restorable, account-bounded scheduler for durable reset jobs.
pub struct ResetResumeScheduler {
    store: Arc<dyn Store>,
    clock: Arc<dyn SchedulerClock>,
    limits: ResetResumeLimits,
    wake: Arc<Notify>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    task: JoinHandle<()>,
}

impl ResetResumeScheduler {
    /// Starts a scheduler that restores pending jobs immediately.
    ///
    /// # Errors
    ///
    /// Rejects an unbounded account set or invalid scheduler limits.
    pub fn start(
        store: Arc<dyn Store>,
        accounts: Arc<[AccountId]>,
        sink: Arc<dyn ResetNotificationSink>,
        clock: Arc<dyn SchedulerClock>,
        limits: ResetResumeLimits,
    ) -> PulseResult<Self> {
        if accounts.len() > MAX_SCHEDULER_ACCOUNTS {
            return Err(PulseError::invalid_input(
                "too many accounts for the reset scheduler",
            ));
        }
        if limits.max_pending_per_account == 0 || limits.max_horizon_millis == 0 {
            return Err(PulseError::invalid_input(
                "reset scheduler limits must be nonzero",
            ));
        }
        let wake = Arc::new(Notify::new());
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = tokio::spawn(run_scheduler(
            Arc::clone(&store),
            accounts,
            sink,
            Arc::clone(&clock),
            limits,
            Arc::clone(&wake),
            Arc::clone(&stopped),
        ));
        Ok(Self {
            store,
            clock,
            limits,
            wake,
            stopped,
            task,
        })
    }

    /// Schedules a typed rate-limit observation and wakes the timer loop.
    ///
    /// # Errors
    ///
    /// Returns validation or persistence failures.
    pub async fn schedule(&self, snapshot: &UsageSnapshot) -> PulseResult<Option<ResetResumeJob>> {
        let job = schedule_rate_limit_resume(
            self.store.as_ref(),
            snapshot,
            self.clock.wall_now(),
            self.limits,
        )
        .await?;
        if job.is_some() {
            self.wake.notify_one();
        }
        Ok(job)
    }

    /// Cancels every pending job for one account/profile.
    ///
    /// # Errors
    ///
    /// Returns a store failure.
    pub async fn cancel(&self, account_id: AccountId, profile: ProfileName) -> PulseResult<usize> {
        let count = self
            .store
            .cancel_reset_resumes(account_id, profile, self.clock.wall_now())
            .await?;
        self.wake.notify_one();
        Ok(count)
    }

    /// Stops the timer task without deleting durable pending jobs.
    pub async fn shutdown(self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        self.wake.notify_waiters();
        let _ = self.task.await;
    }
}

async fn run_scheduler(
    store: Arc<dyn Store>,
    accounts: Arc<[AccountId]>,
    sink: Arc<dyn ResetNotificationSink>,
    clock: Arc<dyn SchedulerClock>,
    limits: ResetResumeLimits,
    wake: Arc<Notify>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
) {
    while !stopped.load(std::sync::atomic::Ordering::Acquire) {
        // Keep the wall and monotonic observations paired. Store I/O can yield;
        // sampling monotonic time after it would add elapsed I/O time twice to
        // a delay calculated from this older wall-clock observation.
        let monotonic_now = clock.monotonic_millis();
        let wall_now = clock.wall_now();
        for account_id in accounts.iter().copied() {
            if sink.channel_available(account_id) {
                deliver_due(store.as_ref(), sink.as_ref(), account_id, wall_now).await;
            }
        }
        let delay = next_delay(
            store.as_ref(),
            &accounts,
            wall_now,
            limits.max_horizon_millis,
        )
        .await
        .unwrap_or(IDLE_RECHECK_MILLIS)
        .min(IDLE_RECHECK_MILLIS);
        let deadline = monotonic_now.saturating_add(delay.max(1));
        tokio::select! {
            () = clock.sleep_until(deadline) => {}
            () = wake.notified() => {}
        }
    }
}

async fn deliver_due(
    store: &dyn Store,
    sink: &dyn ResetNotificationSink,
    account_id: AccountId,
    now: Instant,
) {
    let Ok(lease_until) = checked_add(now, DELIVERY_LEASE_MILLIS) else {
        return;
    };
    let Ok(jobs) = store
        .claim_due_reset_resumes(account_id, now, lease_until, RESTORE_LIMIT)
        .await
    else {
        return;
    };
    for job in jobs {
        let notification = ResetNotification {
            account_id,
            profile: job.input.profile.clone(),
            job_id: job.id,
            resets_at: job.input.resets_at,
            resume_at: job.resume_at,
        };
        if tokio::time::timeout(DELIVERY_TIMEOUT, sink.notify_reset(notification))
            .await
            .is_ok_and(|result| result.is_ok())
        {
            let _ = store.complete_reset_resume(account_id, job.id, now).await;
        }
    }
}

async fn next_delay(
    store: &dyn Store,
    accounts: &[AccountId],
    now: Instant,
    horizon_millis: u64,
) -> Option<u64> {
    let horizon = i64::try_from(horizon_millis).ok()?;
    let through = checked_add(now, horizon).ok()?;
    let mut earliest = None;
    for account_id in accounts.iter().copied() {
        let jobs = store
            .list_pending_reset_resumes(account_id, through, RESTORE_LIMIT)
            .await
            .ok()?;
        if let Some(resume_at) = jobs.first().map(|job| job.resume_at) {
            earliest = Some(earliest.map_or(resume_at, |value: Instant| value.min(resume_at)));
        }
    }
    let delta = earliest?.epoch_millis().saturating_sub(now.epoch_millis());
    Some(u64::try_from(delta).unwrap_or(0))
}

fn checked_add(instant: Instant, millis: i64) -> PulseResult<Instant> {
    let value = instant
        .epoch_millis()
        .checked_add(millis)
        .ok_or_else(|| PulseError::new(PulseErrorKind::InvalidInput, "instant overflowed"))?;
    Instant::from_epoch_millis(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pulse::{
        Account, Machine, MachineName, Percent, Profile, ProfileOrigin, QuotaWindow,
        QuotaWindowKind, RefreshPolicy, Vendor, store::SqliteStore,
    };
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    fn instant(value: i64) -> Instant {
        Instant::from_epoch_millis(value).expect("instant")
    }

    fn snapshot(outcome: CollectionOutcome, resets: &[i64]) -> UsageSnapshot {
        UsageSnapshot {
            account_id: AccountId::new(1).expect("account"),
            profile: ProfileName::new("claude").expect("profile"),
            machine: MachineName::new("max").expect("machine"),
            vendor: Vendor::AnthropicOauth,
            windows: resets
                .iter()
                .enumerate()
                .map(|(index, reset)| QuotaWindow {
                    kind: if index == 0 {
                        QuotaWindowKind::FiveHour
                    } else {
                        QuotaWindowKind::RollingSevenDay
                    },
                    used_percent: Percent::new(100.0).expect("percent"),
                    resets_at: instant(*reset),
                })
                .collect(),
            outcome,
            polled_at: instant(1_000),
            reporter_version: None,
        }
    }

    #[test]
    fn only_rate_limits_choose_the_earliest_future_reset() {
        let limited = snapshot(
            CollectionOutcome::RateLimited { retry_at: None },
            &[900, 3_000, 2_000],
        );
        assert_eq!(
            earliest_future_reset(&limited, instant(1_000)),
            Some(instant(2_000))
        );
        let success = snapshot(CollectionOutcome::Success, &[2_000]);
        assert_eq!(earliest_future_reset(&success, instant(1_000)), None);

        let retry_only = snapshot(
            CollectionOutcome::RateLimited {
                retry_at: Some(instant(1_500)),
            },
            &[],
        );
        assert_eq!(
            earliest_future_reset(&retry_only, instant(1_000)),
            Some(instant(1_500))
        );
        let retry_before_window = snapshot(
            CollectionOutcome::RateLimited {
                retry_at: Some(instant(1_500)),
            },
            &[2_000],
        );
        assert_eq!(
            earliest_future_reset(&retry_before_window, instant(1_000)),
            Some(instant(1_500))
        );
    }

    #[test]
    fn resume_delay_is_exactly_one_minute() {
        let reset = instant(1_000);
        assert_eq!(checked_add(reset, 60_000).expect("resume"), instant(61_000));
    }

    struct FakeClock {
        elapsed: Arc<AtomicU64>,
        base: i64,
        changed: Arc<Notify>,
    }

    impl FakeClock {
        fn new(base: i64) -> Self {
            Self {
                elapsed: Arc::new(AtomicU64::new(0)),
                base,
                changed: Arc::new(Notify::new()),
            }
        }

        fn advance(&self, millis: u64) {
            self.elapsed.fetch_add(millis, Ordering::SeqCst);
            self.changed.notify_waiters();
        }
    }

    impl SchedulerClock for FakeClock {
        fn monotonic_millis(&self) -> u64 {
            self.elapsed.load(Ordering::SeqCst)
        }

        fn wall_now(&self) -> Instant {
            let elapsed = i64::try_from(self.monotonic_millis()).expect("bounded test time");
            instant(self.base + elapsed)
        }

        fn sleep_until(&self, deadline_millis: u64) -> super::super::scheduler::ClockFuture {
            let elapsed = Arc::clone(&self.elapsed);
            let changed = Arc::clone(&self.changed);
            Box::pin(async move {
                loop {
                    let notified = changed.notified();
                    if elapsed.load(Ordering::SeqCst) >= deadline_millis {
                        return;
                    }
                    notified.await;
                }
            })
        }
    }

    struct CountingSink {
        count: AtomicU64,
        delivered: Notify,
    }

    impl ResetNotificationSink for CountingSink {
        fn channel_available(&self, _account_id: AccountId) -> bool {
            true
        }

        fn notify_reset(&self, _notification: ResetNotification) -> ResetDeliveryFuture {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.delivered.notify_one();
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn pending_jobs_restore_after_restart_and_deliver_once() {
        let directory = std::env::temp_dir().join(format!(
            "atmux-reset-restart-{}-{}",
            std::process::id(),
            Instant::now().epoch_millis()
        ));
        std::fs::create_dir(&directory).expect("private reset test directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .expect("secure reset test directory");
        }
        let path = directory.join("pulse.sqlite3");
        let store = Arc::new(SqliteStore::open(&path).await.expect("store"));
        let account_id = AccountId::new(1).expect("account");
        store
            .upsert_account(Account {
                id: account_id,
                identity: "reset@example.test".to_owned(),
                display_name: None,
            })
            .await
            .expect("account");
        store
            .upsert_machine(Machine {
                account_id,
                name: MachineName::new("max").expect("machine"),
                first_seen: instant(1),
                last_seen: instant(1),
            })
            .await
            .expect("machine");
        store
            .upsert_profile(Profile {
                account_id,
                name: ProfileName::new("claude").expect("profile"),
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
        let clock = Arc::new(FakeClock::new(10_000));
        let sink = Arc::new(CountingSink {
            count: AtomicU64::new(0),
            delivered: Notify::new(),
        });
        let first = ResetResumeScheduler::start(
            Arc::clone(&store) as Arc<dyn Store>,
            Arc::from([account_id]),
            Arc::clone(&sink) as Arc<dyn ResetNotificationSink>,
            Arc::clone(&clock) as Arc<dyn SchedulerClock>,
            ResetResumeLimits::default(),
        )
        .expect("first scheduler");
        let mut limited = snapshot(
            CollectionOutcome::RateLimited {
                retry_at: Some(instant(20_000)),
            },
            &[],
        );
        limited.polled_at = instant(10_000);
        first.schedule(&limited).await.expect("schedule");
        first.shutdown().await;
        assert_eq!(sink.count.load(Ordering::SeqCst), 0);

        let second = ResetResumeScheduler::start(
            Arc::clone(&store) as Arc<dyn Store>,
            Arc::from([account_id]),
            Arc::clone(&sink) as Arc<dyn ResetNotificationSink>,
            Arc::clone(&clock) as Arc<dyn SchedulerClock>,
            ResetResumeLimits::default(),
        )
        .expect("restored scheduler");
        tokio::task::yield_now().await;
        let delivered = sink.delivered.notified();
        clock.advance(70_000);
        tokio::time::timeout(Duration::from_secs(2), delivered)
            .await
            .expect("restored job delivered");
        assert_eq!(sink.count.load(Ordering::SeqCst), 1);
        clock.advance(60_000);
        tokio::task::yield_now().await;
        assert_eq!(sink.count.load(Ordering::SeqCst), 1);
        second.shutdown().await;
        drop(store);
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = std::fs::remove_file(candidate);
        }
        std::fs::remove_dir(directory).expect("remove reset test directory");
    }
}
