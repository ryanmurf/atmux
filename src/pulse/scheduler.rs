//! One bounded, jittered scheduler for all native Pulse collection work.

use std::{
    collections::{BTreeSet, HashMap},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant as MonotonicInstant},
};

use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
};

use super::{
    AccountId, ProfileName, PulseError, PulseResult, config::PulseScheduleConfig, time::Instant,
};

const PERIODIC_JOBS: [PulseJob; 5] = [
    PulseJob::Usage,
    PulseJob::Context,
    PulseJob::Tokens,
    PulseJob::Gemini,
    PulseJob::Retention,
];
const MAX_CONCURRENCY: usize = 8;
const IDLE_WAKE_MILLIS: u64 = 24 * 60 * 60 * 1_000;

pub type ClockFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
pub type JobFuture = Pin<Box<dyn Future<Output = PulseResult<JobReport>> + Send + 'static>>;

/// Work categories owned by the single Pulse scheduler.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PulseJob {
    Usage,
    Context,
    Tokens,
    Gemini,
    Retention,
    CompletionPush,
    /// One account- or profile-scoped operator-triggered collection pass.
    ForcePoll(ForcePollTarget),
}

impl PulseJob {
    const fn periodic_index(&self) -> Option<usize> {
        match self {
            Self::Usage => Some(0),
            Self::Context => Some(1),
            Self::Tokens => Some(2),
            Self::Gemini => Some(3),
            Self::Retention => Some(4),
            Self::CompletionPush | Self::ForcePoll(_) => None,
        }
    }
}

/// Validated, secret-free target for one operator-triggered collection pass.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ForcePollTarget {
    pub account_id: AccountId,
    pub profile: Option<ProfileName>,
}

impl ForcePollTarget {
    #[must_use]
    pub const fn account(account_id: AccountId) -> Self {
        Self {
            account_id,
            profile: None,
        }
    }

    #[must_use]
    pub const fn profile(account_id: AccountId, profile: ProfileName) -> Self {
        Self {
            account_id,
            profile: Some(profile),
        }
    }
}

/// Secret-free accounting for one fail-soft job run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JobReport {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
}

impl JobReport {
    #[must_use]
    pub const fn one_success() -> Self {
        Self {
            attempted: 1,
            succeeded: 1,
            failed: 0,
        }
    }

    #[must_use]
    pub const fn one_failure() -> Self {
        Self {
            attempted: 1,
            succeeded: 0,
            failed: 1,
        }
    }

    #[must_use]
    pub const fn combine(self, other: Self) -> Self {
        Self {
            attempted: self.attempted.saturating_add(other.attempted),
            succeeded: self.succeeded.saturating_add(other.succeeded),
            failed: self.failed.saturating_add(other.failed),
        }
    }
}

/// Clock boundary used by production and deterministic scheduler tests.
pub trait SchedulerClock: Send + Sync + 'static {
    fn monotonic_millis(&self) -> u64;
    fn wall_now(&self) -> Instant;
    fn sleep_until(&self, deadline_millis: u64) -> ClockFuture;
}

/// Production monotonic/wall clock.
#[derive(Debug)]
pub struct SystemClock {
    origin: MonotonicInstant,
}

impl SystemClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: MonotonicInstant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedulerClock for SystemClock {
    fn monotonic_millis(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn wall_now(&self) -> Instant {
        Instant::now()
    }

    fn sleep_until(&self, deadline_millis: u64) -> ClockFuture {
        let now = self.monotonic_millis();
        let delay = Duration::from_millis(deadline_millis.saturating_sub(now));
        Box::pin(tokio::time::sleep(delay))
    }
}

/// Typed asynchronous job implementation.
pub trait JobRunner: Send + Sync + 'static {
    fn run(&self, job: PulseJob, triggered_at: Instant) -> JobFuture;
}

/// Validated scheduler intervals in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerSettings {
    intervals_millis: [u64; 5],
    enabled: [bool; 5],
    completion_after_collection: bool,
    jitter_percent: u8,
    max_concurrency: usize,
}

impl SchedulerSettings {
    /// Builds deterministic settings. Public construction permits fake-clock
    /// tests without weakening user-facing configuration cadence floors.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for zero/overflowed intervals, jitter
    /// over 50%, or concurrency outside 1–8.
    pub fn new(
        intervals_millis: [u64; 5],
        jitter_percent: u8,
        max_concurrency: usize,
    ) -> PulseResult<Self> {
        if intervals_millis.contains(&0) {
            return Err(PulseError::configuration(
                "Pulse scheduler intervals must be nonzero",
            ));
        }
        if jitter_percent > 50 {
            return Err(PulseError::configuration(
                "Pulse scheduler jitter cannot exceed 50 percent",
            ));
        }
        if !(1..=MAX_CONCURRENCY).contains(&max_concurrency) {
            return Err(PulseError::configuration(
                "Pulse scheduler concurrency must be between 1 and 8",
            ));
        }
        Ok(Self {
            intervals_millis,
            enabled: [true; 5],
            completion_after_collection: false,
            jitter_percent,
            max_concurrency,
        })
    }

    /// Converts the user configuration after its cadence-floor validation.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an invalid schedule or overflow.
    pub fn from_config(config: &PulseScheduleConfig) -> PulseResult<Self> {
        config.validate()?;
        let seconds = [
            config.usage,
            config.context,
            config.tokens,
            config.gemini,
            config.retention,
        ];
        let mut intervals = [0_u64; 5];
        for (target, seconds) in intervals.iter_mut().zip(seconds) {
            *target = seconds
                .checked_mul(1_000)
                .ok_or_else(|| PulseError::configuration("Pulse scheduler interval overflowed"))?;
        }
        Self::new(intervals, config.jitter_percent, 2)
    }

    const fn interval(self, job: &PulseJob) -> Option<u64> {
        let Some(index) = job.periodic_index() else {
            return None;
        };
        Some(self.intervals_millis[index])
    }

    pub(crate) const fn with_capabilities(
        mut self,
        collect: bool,
        completion_after_collection: bool,
    ) -> Self {
        self.enabled = [collect, collect, collect, collect, true];
        self.completion_after_collection = collect && completion_after_collection;
        self
    }

    const fn is_enabled(self, job: &PulseJob) -> bool {
        match job.periodic_index() {
            Some(index) => self.enabled[index],
            None => true,
        }
    }
}

/// Handle for completion-trigger coalescing and clean shutdown.
pub struct SchedulerHandle {
    commands: mpsc::Sender<SchedulerCommand>,
    task: JoinHandle<()>,
}

/// Cloneable command-only view of the sole scheduler.
#[derive(Clone)]
pub struct SchedulerClient {
    commands: mpsc::Sender<SchedulerCommand>,
}

impl SchedulerClient {
    /// Queues one account- or profile-scoped force poll on the existing scheduler.
    /// Repeated requests for an already queued/running target coalesce.
    ///
    /// # Errors
    ///
    /// Returns rate-limited when the bounded command queue is full, or a
    /// conflict after scheduler shutdown.
    pub fn notify_force_poll(&self, target: ForcePollTarget) -> PulseResult<()> {
        notify_force_poll(&self.commands, target)
    }
}

impl SchedulerHandle {
    /// Returns a command-only client that cannot stop or replace the scheduler.
    #[must_use]
    pub fn client(&self) -> SchedulerClient {
        SchedulerClient {
            commands: self.commands.clone(),
        }
    }

    /// Queues one completion-triggered push without blocking the caller.
    /// Multiple notifications coalesce while a push is running or queued.
    ///
    /// # Errors
    ///
    /// Returns a conflict error if the scheduler already stopped.
    pub fn notify_completion(&self) -> PulseResult<()> {
        match self.commands.try_send(SchedulerCommand::Completion) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(PulseError::new(
                super::PulseErrorKind::Conflict,
                "Pulse scheduler stopped",
            )),
        }
    }

    /// Queues one account- or profile-scoped force poll on the existing scheduler.
    /// Repeated requests for an already queued/running target coalesce.
    ///
    /// # Errors
    ///
    /// Returns rate-limited when the bounded command queue is full, or a
    /// conflict after scheduler shutdown.
    pub fn notify_force_poll(&self, target: ForcePollTarget) -> PulseResult<()> {
        notify_force_poll(&self.commands, target)
    }

    /// Cancels sleeping/running jobs and waits until every task is gone.
    pub async fn shutdown(self) {
        let (acknowledge, acknowledged) = oneshot::channel();
        let _ = self
            .commands
            .send(SchedulerCommand::Shutdown(acknowledge))
            .await;
        let _ = acknowledged.await;
        let _ = self.task.await;
    }
}

fn notify_force_poll(
    commands: &mpsc::Sender<SchedulerCommand>,
    target: ForcePollTarget,
) -> PulseResult<()> {
    match commands.try_send(SchedulerCommand::ForcePoll(target)) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => Err(PulseError::new(
            super::PulseErrorKind::RateLimited,
            "Pulse force-poll queue is full",
        )),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(PulseError::new(
            super::PulseErrorKind::Conflict,
            "Pulse scheduler stopped",
        )),
    }
}

enum SchedulerCommand {
    Completion,
    ForcePoll(ForcePollTarget),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone, Copy, Debug)]
struct JobState {
    running: bool,
    next_due_millis: u64,
    generation: u64,
}

#[derive(Debug)]
struct ScheduleState {
    periodic: [JobState; 5],
    completion_running: bool,
    completion_queued: bool,
    force_running: Option<ForcePollTarget>,
    force_queued: BTreeSet<ForcePollTarget>,
    settings: SchedulerSettings,
    jitter_seed: u64,
}

impl ScheduleState {
    fn new(now_millis: u64, settings: SchedulerSettings, jitter_seed: u64) -> Self {
        Self {
            periodic: std::array::from_fn(|index| JobState {
                running: false,
                next_due_millis: if settings.enabled[index] {
                    now_millis
                } else {
                    u64::MAX
                },
                generation: 0,
            }),
            completion_running: false,
            completion_queued: false,
            force_running: None,
            force_queued: BTreeSet::new(),
            settings,
            jitter_seed,
        }
    }

    fn take_due(&mut self, now_millis: u64) -> Vec<PulseJob> {
        let mut due = Vec::with_capacity(6);
        for job in PERIODIC_JOBS {
            if !self.settings.is_enabled(&job) {
                continue;
            }
            if self.force_running.is_some() && job != PulseJob::Retention {
                continue;
            }
            let state = &mut self.periodic[job.periodic_index().expect("periodic job")];
            if !state.running && now_millis >= state.next_due_millis {
                state.running = true;
                due.push(job);
            }
        }
        if self.completion_ready() {
            self.completion_queued = false;
            self.completion_running = true;
            due.push(PulseJob::CompletionPush);
        }
        if self.force_ready()
            && let Some(target) = self.force_queued.pop_first()
        {
            self.force_running = Some(target.clone());
            due.push(PulseJob::ForcePoll(target));
        }
        due
    }

    fn completion_triggered(&mut self) {
        self.completion_queued = true;
    }

    fn completion_ready(&self) -> bool {
        self.completion_queued
            && !self.completion_running
            && !self.periodic.iter().any(|state| state.running)
            && self.force_running.is_none()
    }

    fn force_triggered(&mut self, target: ForcePollTarget) {
        if self.force_running.as_ref() != Some(&target) {
            self.force_queued.insert(target);
        }
    }

    fn force_ready(&self) -> bool {
        !self.force_queued.is_empty()
            && self.force_running.is_none()
            && !self.completion_running
            && !self.periodic[..4].iter().any(|state| state.running)
    }

    fn collection_finished_with_rows(&mut self, job: &PulseJob, report: &PulseResult<JobReport>) {
        if self.settings.completion_after_collection
            && matches!(
                job,
                PulseJob::Usage
                    | PulseJob::Context
                    | PulseJob::Tokens
                    | PulseJob::Gemini
                    | PulseJob::ForcePoll(_)
            )
            && report.as_ref().is_ok_and(|report| report.succeeded > 0)
        {
            self.completion_triggered();
        }
    }

    fn finished(&mut self, job: &PulseJob, finished_millis: u64) {
        if let Some(index) = job.periodic_index() {
            let state = &mut self.periodic[index];
            state.running = false;
            state.generation = state.generation.saturating_add(1);
            let interval = self.settings.interval(job).expect("periodic interval");
            let jittered = jittered_interval(
                interval,
                self.settings.jitter_percent,
                self.jitter_seed
                    ^ u64::try_from(index).unwrap_or(0)
                    ^ state.generation.rotate_left(17),
            );
            state.next_due_millis = finished_millis.saturating_add(jittered);
        } else {
            match job {
                PulseJob::CompletionPush => self.completion_running = false,
                PulseJob::ForcePoll(_) => self.force_running = None,
                PulseJob::Usage
                | PulseJob::Context
                | PulseJob::Tokens
                | PulseJob::Gemini
                | PulseJob::Retention => unreachable!("periodic job has an index"),
            }
        }
    }

    fn next_deadline(&self, now_millis: u64) -> u64 {
        if self.completion_ready() || self.force_ready() {
            return now_millis;
        }
        self.periodic
            .iter()
            .filter(|state| !state.running)
            .map(|state| state.next_due_millis)
            .min()
            .unwrap_or_else(|| now_millis.saturating_add(IDLE_WAKE_MILLIS))
    }
}

/// Starts the sole scheduler task. All five periodic jobs run once at startup,
/// then use their configured jittered interval measured from completion.
#[must_use]
pub fn spawn_scheduler(
    runner: Arc<dyn JobRunner>,
    clock: Arc<dyn SchedulerClock>,
    settings: SchedulerSettings,
    jitter_seed: u64,
) -> SchedulerHandle {
    let (commands, receiver) = mpsc::channel(32);
    let task = tokio::spawn(run_scheduler(
        runner,
        clock,
        settings,
        jitter_seed,
        receiver,
    ));
    SchedulerHandle { commands, task }
}

async fn run_scheduler(
    runner: Arc<dyn JobRunner>,
    clock: Arc<dyn SchedulerClock>,
    settings: SchedulerSettings,
    jitter_seed: u64,
    mut commands: mpsc::Receiver<SchedulerCommand>,
) {
    let permits = Arc::new(tokio::sync::Semaphore::new(settings.max_concurrency));
    let mut schedule = ScheduleState::new(clock.monotonic_millis(), settings, jitter_seed);
    let mut tasks = JoinSet::new();
    let mut task_jobs = HashMap::new();
    loop {
        for job in schedule.take_due(clock.monotonic_millis()) {
            let runner = Arc::clone(&runner);
            let clock = Arc::clone(&clock);
            let permits = Arc::clone(&permits);
            let running_job = job.clone();
            let task = tasks.spawn(async move {
                let Ok(_permit) = permits.acquire_owned().await else {
                    return (
                        running_job,
                        Err(PulseError::new(
                            super::PulseErrorKind::Internal,
                            "Pulse scheduler permit pool closed",
                        )),
                    );
                };
                let result = runner.run(running_job.clone(), clock.wall_now()).await;
                (running_job, result)
            });
            task_jobs.insert(task.id(), job);
        }

        let deadline = schedule.next_deadline(clock.monotonic_millis());
        tokio::select! {
            () = clock.sleep_until(deadline) => {}
            command = commands.recv() => match command {
                Some(SchedulerCommand::Completion) => schedule.completion_triggered(),
                Some(SchedulerCommand::ForcePoll(target)) => schedule.force_triggered(target),
                Some(SchedulerCommand::Shutdown(acknowledge)) => {
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    let _ = acknowledge.send(());
                    return;
                }
                None => {
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    return;
                }
            },
            completed = tasks.join_next_with_id(), if !tasks.is_empty() => {
                match completed {
                    Some(Ok((id, (job, result)))) => {
                        task_jobs.remove(&id);
                        schedule.finished(&job, clock.monotonic_millis());
                        schedule.collection_finished_with_rows(&job, &result);
                    }
                    Some(Err(error)) => {
                        if let Some(job) = task_jobs.remove(&error.id()) {
                            schedule.finished(&job, clock.monotonic_millis());
                        }
                    }
                    None => {}
                }
            }
        }
    }
}

fn jittered_interval(interval: u64, jitter_percent: u8, sample: u64) -> u64 {
    let spread = interval
        .saturating_mul(u64::from(jitter_percent))
        .checked_div(100)
        .unwrap_or(0);
    if spread == 0 {
        return interval;
    }
    let width = spread.saturating_mul(2).saturating_add(1);
    let mixed = splitmix64(sample);
    let position = mixed % width;
    if position >= spread {
        interval.saturating_add(position - spread)
    } else {
        interval.saturating_sub(spread - position)
    }
}

const fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use std::{
        future,
        sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    };

    use tokio::sync::Notify;

    use super::*;

    struct SharedFakeClock {
        now: Arc<AtomicU64>,
        wall: Instant,
        changed: Arc<Notify>,
    }

    impl SharedFakeClock {
        fn new() -> Self {
            Self {
                now: Arc::new(AtomicU64::new(0)),
                wall: Instant::from_epoch_millis(1_786_214_400_000).expect("valid wall time"),
                changed: Arc::new(Notify::new()),
            }
        }

        fn advance(&self, millis: u64) {
            self.now.fetch_add(millis, Ordering::SeqCst);
            self.changed.notify_waiters();
        }
    }

    impl SchedulerClock for SharedFakeClock {
        fn monotonic_millis(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }

        fn wall_now(&self) -> Instant {
            self.wall
        }

        fn sleep_until(&self, deadline_millis: u64) -> ClockFuture {
            let now = Arc::clone(&self.now);
            let changed = Arc::clone(&self.changed);
            Box::pin(async move {
                loop {
                    let notified = changed.notified();
                    if now.load(Ordering::SeqCst) >= deadline_millis {
                        return;
                    }
                    notified.await;
                }
            })
        }
    }

    struct ImmediateRunner {
        counts: [AtomicUsize; 7],
        fail: bool,
    }

    impl ImmediateRunner {
        fn new(fail: bool) -> Self {
            Self {
                counts: std::array::from_fn(|_| AtomicUsize::new(0)),
                fail,
            }
        }

        fn count(&self, job: &PulseJob) -> usize {
            self.counts[job_index(job)].load(Ordering::SeqCst)
        }
    }

    impl JobRunner for ImmediateRunner {
        fn run(&self, job: PulseJob, _triggered_at: Instant) -> JobFuture {
            self.counts[job_index(&job)].fetch_add(1, Ordering::SeqCst);
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(PulseError::new(
                        crate::pulse::PulseErrorKind::Upstream,
                        "fake collector unavailable",
                    ))
                } else {
                    Ok(JobReport::one_success())
                }
            })
        }
    }

    struct PendingRunner {
        started: AtomicUsize,
    }

    impl JobRunner for PendingRunner {
        fn run(&self, _job: PulseJob, _triggered_at: Instant) -> JobFuture {
            self.started.fetch_add(1, Ordering::SeqCst);
            Box::pin(future::pending())
        }
    }

    struct PanicOnceRunner {
        usage_runs: AtomicUsize,
    }

    impl JobRunner for PanicOnceRunner {
        fn run(&self, job: PulseJob, _triggered_at: Instant) -> JobFuture {
            let should_panic =
                job == PulseJob::Usage && self.usage_runs.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                assert!(!should_panic, "intentional fake collector panic");
                Ok(JobReport::one_success())
            })
        }
    }

    fn job_index(job: &PulseJob) -> usize {
        match job {
            PulseJob::Usage => 0,
            PulseJob::Context => 1,
            PulseJob::Tokens => 2,
            PulseJob::Gemini => 3,
            PulseJob::Retention => 4,
            PulseJob::CompletionPush => 5,
            PulseJob::ForcePoll(_) => 6,
        }
    }

    #[test]
    fn settings_preserve_the_authoritative_default_cadences() {
        let settings = SchedulerSettings::from_config(&PulseScheduleConfig::default())
            .expect("default settings");
        assert_eq!(
            settings.intervals_millis,
            [900_000, 120_000, 1_800_000, 1_800_000, 3_600_000]
        );
        assert_eq!(settings.jitter_percent, 10);
    }

    #[test]
    fn jitter_is_deterministic_and_stays_inside_its_symmetric_bound() {
        for sample in 0..10_000 {
            let value = jittered_interval(1_000, 10, sample);
            assert!((900..=1_100).contains(&value));
            assert_eq!(value, jittered_interval(1_000, 10, sample));
        }
        assert_eq!(jittered_interval(1_000, 0, 9), 1_000);
    }

    #[test]
    fn schedule_is_single_flight_and_completion_notifications_coalesce() {
        let settings = SchedulerSettings::new([100; 5], 0, 2).expect("settings");
        let mut state = ScheduleState::new(0, settings, 1);
        assert_eq!(state.take_due(0), PERIODIC_JOBS);
        assert!(state.take_due(0).is_empty());

        state.completion_triggered();
        state.completion_triggered();
        assert!(state.take_due(0).is_empty());
        for job in PERIODIC_JOBS {
            state.finished(&job, 0);
        }
        assert_eq!(state.take_due(0), vec![PulseJob::CompletionPush]);
        assert!(state.take_due(0).is_empty());
        state.completion_triggered();
        state.finished(&PulseJob::CompletionPush, 0);
        assert_eq!(state.take_due(0), vec![PulseJob::CompletionPush]);

        state.finished(&PulseJob::Usage, 10);
        assert!(!state.take_due(109).contains(&PulseJob::Usage));
        assert!(state.take_due(110).contains(&PulseJob::Usage));
    }

    #[test]
    fn account_force_notifications_coalesce_and_serialize() {
        let settings = SchedulerSettings::new([100; 5], 0, 2).expect("settings");
        let mut state = ScheduleState::new(0, settings, 1);
        for job in state.take_due(0) {
            state.finished(&job, 0);
        }
        let one = AccountId::new(1).expect("account one");
        let two = AccountId::new(2).expect("account two");
        let one_all = ForcePollTarget::account(one);
        let one_profile =
            ForcePollTarget::profile(one, ProfileName::new("claude-max").expect("profile"));
        let two_all = ForcePollTarget::account(two);
        state.force_triggered(one_all.clone());
        state.force_triggered(one_all.clone());
        state.force_triggered(one_profile.clone());
        state.force_triggered(two_all.clone());
        assert_eq!(
            state.take_due(0),
            vec![PulseJob::ForcePoll(one_all.clone())]
        );
        assert!(state.take_due(0).is_empty());
        state.force_triggered(one_all.clone());
        state.finished(&PulseJob::ForcePoll(one_all), 1);
        assert_eq!(
            state.take_due(1),
            vec![PulseJob::ForcePoll(one_profile.clone())]
        );
        state.finished(&PulseJob::ForcePoll(one_profile), 2);
        assert_eq!(
            state.take_due(2),
            vec![PulseJob::ForcePoll(two_all.clone())]
        );
        state.finished(&PulseJob::ForcePoll(two_all), 3);
        assert!(state.take_due(2).is_empty());
    }

    #[test]
    fn api_only_capabilities_schedule_retention_and_no_collectors() {
        let settings = SchedulerSettings::new([100; 5], 0, 2)
            .expect("settings")
            .with_capabilities(false, false);
        let mut state = ScheduleState::new(0, settings, 1);
        assert_eq!(state.take_due(0), vec![PulseJob::Retention]);
        state.finished(&PulseJob::Retention, 0);
        assert!(state.take_due(99).is_empty());
        assert_eq!(state.take_due(100), vec![PulseJob::Retention]);
    }

    #[test]
    fn successful_collection_completion_coalesces_one_report_after_the_batch() {
        let settings = SchedulerSettings::new([100; 5], 0, 2)
            .expect("settings")
            .with_capabilities(true, true);
        let mut state = ScheduleState::new(0, settings, 1);
        assert_eq!(state.take_due(0), PERIODIC_JOBS);
        for job in PERIODIC_JOBS {
            state.finished(&job, 0);
            state.collection_finished_with_rows(&job, &Ok(JobReport::one_success()));
            if job != PulseJob::Retention {
                assert!(state.take_due(0).is_empty());
            }
        }
        assert_eq!(state.take_due(0), vec![PulseJob::CompletionPush]);
    }

    #[tokio::test]
    async fn failures_reschedule_and_fake_clock_advances_without_sleeping() {
        let clock = Arc::new(SharedFakeClock::new());
        let runner = Arc::new(ImmediateRunner::new(true));
        let handle = spawn_scheduler(
            runner.clone(),
            clock.clone(),
            SchedulerSettings::new([100; 5], 0, 2).expect("settings"),
            0,
        );
        wait_until(|| runner.count(&PulseJob::Usage) == 1).await;
        clock.advance(100);
        wait_until(|| runner.count(&PulseJob::Usage) == 2).await;
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn concurrency_is_bounded_and_shutdown_aborts_pending_jobs() {
        let clock = Arc::new(SharedFakeClock::new());
        let runner = Arc::new(PendingRunner {
            started: AtomicUsize::new(0),
        });
        let handle = spawn_scheduler(
            runner.clone(),
            clock,
            SchedulerSettings::new([100; 5], 0, 2).expect("settings"),
            0,
        );
        wait_until(|| runner.started.load(Ordering::SeqCst) == 2).await;
        assert_eq!(runner.started.load(Ordering::SeqCst), 2);
        tokio::time::timeout(Duration::from_secs(1), handle.shutdown())
            .await
            .expect("clean shutdown");
    }

    #[tokio::test]
    async fn panicking_job_is_released_and_runs_again() {
        let clock = Arc::new(SharedFakeClock::new());
        let runner = Arc::new(PanicOnceRunner {
            usage_runs: AtomicUsize::new(0),
        });
        let handle = spawn_scheduler(
            runner.clone(),
            clock.clone(),
            SchedulerSettings::new([100; 5], 0, 2).expect("settings"),
            0,
        );
        wait_until(|| runner.usage_runs.load(Ordering::SeqCst) == 1).await;
        // Let the JoinError path release the single-flight bit before the next
        // deadline becomes due.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        clock.advance(100);
        wait_until(|| runner.usage_runs.load(Ordering::SeqCst) == 2).await;
        handle.shutdown().await;
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..1_000 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition did not become true");
    }
}
