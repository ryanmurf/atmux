//! Bounded account-scoped Pulse reports, usage pace, and context capacity.

use std::collections::{BTreeMap, BTreeSet};

use jiff::{Span, civil::Date};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    AccountId, ContextSession, Instant, MachineName, Percent, ProfileName, PulseError,
    PulseErrorKind, PulseResult, QuotaWindowKind, TokenGrain, Vendor,
    pricing::{PricingOrigin, cost_for_grain, effective_default_pricing, resolve_vendor_pricing},
    store::{CurrentQuotaWindow, PricingRule, Store},
};

pub const MAX_REPORT_DAYS: u16 = 365;
pub const MAX_REPORT_ROWS: usize = 9_999;
pub const MAX_PACE_PROFILES: usize = 512;
pub const MAX_CONTEXT_SESSIONS: usize = 5_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportGranularity {
    #[default]
    Daily,
    Weekly,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportDrill {
    #[default]
    Profile,
    Machine,
    Session,
    Model,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportRange {
    pub since_day: String,
    pub through_day: String,
}

impl ReportRange {
    /// Builds an inclusive range ending on `through_day`.
    ///
    /// # Errors
    ///
    /// Rejects zero or more than 365 days and invalid civil dates.
    pub fn recent(through_day: &str, days: u16) -> PulseResult<Self> {
        if days == 0 || days > MAX_REPORT_DAYS {
            return Err(PulseError::invalid_input(format!(
                "report days must be between 1 and {MAX_REPORT_DAYS}"
            )));
        }
        let through = parse_day(through_day)?;
        let since = through
            .checked_sub(Span::new().days(i64::from(days - 1)))
            .map_err(|error| {
                PulseError::invalid_input(format!("report range overflowed: {error}"))
            })?;
        Ok(Self {
            since_day: since.to_string(),
            through_day: through.to_string(),
        })
    }

    fn validate(&self) -> PulseResult<()> {
        let since = parse_day(&self.since_day)?;
        let through = parse_day(&self.through_day)?;
        if since > through {
            return Err(PulseError::invalid_input(
                "report since_day must not be after through_day",
            ));
        }
        let mut cursor = since;
        let mut days = 1_u16;
        while cursor < through {
            if days >= MAX_REPORT_DAYS {
                return Err(PulseError::invalid_input(format!(
                    "report date range cannot exceed {MAX_REPORT_DAYS} days"
                )));
            }
            cursor = cursor.checked_add(Span::new().days(1)).map_err(|error| {
                PulseError::invalid_input(format!("report range overflowed: {error}"))
            })?;
            days = days.saturating_add(1);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenReportRequest {
    pub account_id: AccountId,
    pub range: ReportRange,
    pub granularity: ReportGranularity,
    pub drill: ReportDrill,
    pub profile: Option<ProfileName>,
    pub machine: Option<MachineName>,
    pub exclude_profiles: BTreeSet<ProfileName>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportTotals {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportBreakdown {
    pub key: String,
    #[serde(flatten)]
    pub totals: ReportTotals,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportPeriod {
    pub day: String,
    #[serde(flatten)]
    pub totals: ReportTotals,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileTokenReport {
    pub profile: ProfileName,
    #[serde(flatten)]
    pub totals: ReportTotals,
    pub by_period: Vec<ReportPeriod>,
    pub by_machine: Vec<ReportBreakdown>,
    pub drill: Vec<ReportBreakdown>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TokenReport {
    pub range: ReportRange,
    pub granularity: ReportGranularity,
    pub drill: ReportDrill,
    pub profiles: Vec<ProfileTokenReport>,
    pub total: ReportTotals,
    pub rows_scanned: usize,
    pub fallback_priced_rows: usize,
}

/// Reads bounded SQL-backed grains and re-prices them under current defaults
/// and account overrides.
///
/// # Errors
///
/// Rejects invalid date/work bounds, arithmetic overflow, and store failures.
pub async fn token_report(
    store: &dyn Store,
    request: TokenReportRequest,
) -> PulseResult<TokenReport> {
    request.range.validate()?;
    if request
        .profile
        .as_ref()
        .is_some_and(|profile| request.exclude_profiles.contains(profile))
    {
        return Err(PulseError::invalid_input(
            "an explicitly requested profile cannot also be excluded",
        ));
    }
    let rows = store
        .list_token_grains(
            request.account_id,
            request.profile.clone(),
            Some(request.range.since_day.clone()),
            MAX_REPORT_ROWS + 1,
        )
        .await?;
    if rows.len() > MAX_REPORT_ROWS {
        return Err(PulseError::new(
            PulseErrorKind::Storage,
            "token report exceeded its SQL row bound",
        ));
    }
    let defaults = effective_default_pricing(&store.list_pricing_defaults().await?);
    let overrides = store.list_pricing_overrides(request.account_id).await?;
    let vendors = store
        .list_profiles(request.account_id)
        .await?
        .into_iter()
        .map(|profile| (profile.name, profile.vendor))
        .collect::<BTreeMap<_, _>>();
    if vendors.len() > MAX_PACE_PROFILES {
        return Err(PulseError::new(
            PulseErrorKind::Storage,
            "token report exceeded its profile work bound",
        ));
    }
    build_report(request, rows, &defaults, &overrides, &vendors)
}

fn build_report(
    request: TokenReportRequest,
    rows: Vec<TokenGrain>,
    defaults: &[PricingRule],
    overrides: &[PricingRule],
    vendors: &BTreeMap<ProfileName, Vendor>,
) -> PulseResult<TokenReport> {
    if rows.len() > MAX_REPORT_ROWS {
        return Err(PulseError::new(
            PulseErrorKind::Storage,
            "token report exceeded its work bound",
        ));
    }
    let mut profiles = BTreeMap::<ProfileName, ProfileAccumulator>::new();
    let mut total = ReportTotals::default();
    let mut rows_scanned = 0_usize;
    let mut fallback_priced_rows = 0_usize;
    for row in rows {
        if row.account_id != request.account_id
            || row.day < request.range.since_day
            || row.day > request.range.through_day
            || request
                .machine
                .as_ref()
                .is_some_and(|machine| &row.machine != machine)
            || request.exclude_profiles.contains(&row.profile)
        {
            continue;
        }
        rows_scanned = rows_scanned.saturating_add(1);
        let vendor = vendors.get(&row.profile).copied().ok_or_else(|| {
            PulseError::new(
                PulseErrorKind::Storage,
                "token report row referenced an unavailable profile",
            )
        })?;
        let resolved =
            resolve_vendor_pricing(vendor, &row.model, &row.settings, defaults, overrides);
        fallback_priced_rows = fallback_priced_rows
            .saturating_add(usize::from(resolved.origin == PricingOrigin::Fallback));
        let cost = cost_for_grain(&row, resolved.rate)?;
        total.add(&row, cost)?;
        profiles.entry(row.profile.clone()).or_default().add(
            &row,
            cost,
            request.granularity,
            request.drill,
        )?;
    }
    let mut profile_reports = profiles
        .into_iter()
        .map(|(profile, accumulator)| accumulator.finish(profile))
        .collect::<Vec<_>>();
    profile_reports.sort_by(|left, right| {
        right
            .totals
            .cost_usd
            .total_cmp(&left.totals.cost_usd)
            .then_with(|| left.profile.cmp(&right.profile))
    });
    total.round_cost();
    Ok(TokenReport {
        range: request.range,
        granularity: request.granularity,
        drill: request.drill,
        profiles: profile_reports,
        total,
        rows_scanned,
        fallback_priced_rows,
    })
}

impl ReportTotals {
    fn add(&mut self, row: &TokenGrain, cost: f64) -> PulseResult<()> {
        self.tokens_in = checked_add(self.tokens_in, row.tokens_in)?;
        self.tokens_out = checked_add(self.tokens_out, row.tokens_out)?;
        self.cache_write_5m = checked_add(self.cache_write_5m, row.cache_write_5m)?;
        self.cache_write_1h = checked_add(self.cache_write_1h, row.cache_write_1h)?;
        self.cache_read = checked_add(self.cache_read, row.cache_read)?;
        let row_total = checked_add(
            checked_add(
                checked_add(row.tokens_in, row.tokens_out)?,
                checked_add(row.cache_write_5m, row.cache_write_1h)?,
            )?,
            row.cache_read,
        )?;
        self.total_tokens = checked_add(self.total_tokens, row_total)?;
        self.cost_usd += cost;
        if !self.cost_usd.is_finite() {
            return Err(PulseError::invalid_input("token report cost overflowed"));
        }
        Ok(())
    }

    fn round_cost(&mut self) {
        self.cost_usd = (self.cost_usd * 1_000_000.0).round() / 1_000_000.0;
    }
}

fn checked_add(left: u64, right: u64) -> PulseResult<u64> {
    left.checked_add(right)
        .ok_or_else(|| PulseError::invalid_input("token report total overflowed"))
}

#[derive(Default)]
struct ProfileAccumulator {
    totals: ReportTotals,
    periods: BTreeMap<String, ReportTotals>,
    machines: BTreeMap<String, ReportTotals>,
    drill: BTreeMap<String, ReportTotals>,
}

impl ProfileAccumulator {
    fn add(
        &mut self,
        row: &TokenGrain,
        cost: f64,
        granularity: ReportGranularity,
        drill: ReportDrill,
    ) -> PulseResult<()> {
        self.totals.add(row, cost)?;
        let bucket = match granularity {
            ReportGranularity::Daily => row.day.clone(),
            ReportGranularity::Weekly => week_bucket(&row.day)?,
        };
        self.periods.entry(bucket).or_default().add(row, cost)?;
        self.machines
            .entry(row.machine.as_str().to_owned())
            .or_default()
            .add(row, cost)?;
        if drill != ReportDrill::Profile {
            let key = match drill {
                ReportDrill::Profile => unreachable!("profile drill is handled above"),
                ReportDrill::Machine => row.machine.as_str(),
                ReportDrill::Session => row.session_id.as_str(),
                ReportDrill::Model => &row.model,
            };
            self.drill
                .entry(key.to_owned())
                .or_default()
                .add(row, cost)?;
        }
        Ok(())
    }

    fn finish(mut self, profile: ProfileName) -> ProfileTokenReport {
        self.totals.round_cost();
        let by_period = self
            .periods
            .into_iter()
            .map(|(day, mut totals)| {
                totals.round_cost();
                ReportPeriod { day, totals }
            })
            .collect();
        let by_machine = finish_breakdown(self.machines);
        let drill = finish_breakdown(self.drill);
        ProfileTokenReport {
            profile,
            totals: self.totals,
            by_period,
            by_machine,
            drill,
        }
    }
}

fn finish_breakdown(values: BTreeMap<String, ReportTotals>) -> Vec<ReportBreakdown> {
    let mut rows = values
        .into_iter()
        .map(|(key, mut totals)| {
            totals.round_cost();
            ReportBreakdown { key, totals }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .totals
            .cost_usd
            .total_cmp(&left.totals.cost_usd)
            .then_with(|| left.key.cmp(&right.key))
    });
    rows
}

fn parse_day(day: &str) -> PulseResult<Date> {
    day.parse::<Date>()
        .map_err(|error| PulseError::invalid_input(format!("invalid report day: {error}")))
}

fn week_bucket(day: &str) -> PulseResult<String> {
    let day = parse_day(day)?;
    let offset = i64::from(day.weekday().to_monday_zero_offset());
    day.checked_sub(Span::new().days(offset))
        .map(|monday| monday.to_string())
        .map_err(|error| PulseError::invalid_input(format!("weekly bucket overflowed: {error}")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaceBand {
    Stale,
    Conserve,
    SlightlyFast,
    OnTrack,
    CapacityAvailable,
    DurationUnavailable,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsagePace {
    pub profile: ProfileName,
    pub window: QuotaWindowKind,
    pub used_percent: Percent,
    pub capacity_percent: f64,
    pub remaining_ms: i64,
    pub elapsed_percent: Option<f64>,
    pub projected_used_percent: Option<f64>,
    pub band: PaceBand,
    pub chosen_machines: Vec<MachineName>,
}

/// Computes deterministic pace/capacity from account-global current windows.
///
/// # Errors
///
/// Returns store failures or a work-bound error for excessive profiles.
pub async fn usage_pace(
    store: &dyn Store,
    account_id: AccountId,
    profile: Option<ProfileName>,
    now: Instant,
) -> PulseResult<Vec<UsagePace>> {
    let profiles = if let Some(profile) = profile {
        vec![profile]
    } else {
        store
            .list_profiles(account_id)
            .await?
            .into_iter()
            .filter(|profile| !profile.hidden)
            .map(|profile| profile.name)
            .collect()
    };
    if profiles.len() > MAX_PACE_PROFILES {
        return Err(PulseError::new(
            PulseErrorKind::Storage,
            "usage pace exceeded its profile work bound",
        ));
    }
    let mut rows = Vec::new();
    for profile in profiles {
        for current in store.current_usage(account_id, profile).await? {
            rows.push(pace_for_window(current, now));
        }
    }
    rows.sort_by(|left, right| {
        left.profile
            .cmp(&right.profile)
            .then_with(|| (left.window as u8).cmp(&(right.window as u8)))
    });
    Ok(rows)
}

fn pace_for_window(current: CurrentQuotaWindow, now: Instant) -> UsagePace {
    let used = current.window.used_percent.get();
    let remaining_ms = current
        .window
        .resets_at
        .epoch_millis()
        .saturating_sub(now.epoch_millis());
    let duration = window_duration_ms(current.window.kind);
    let (elapsed_percent, projected_used_percent, band) = if remaining_ms <= 0 {
        (None, None, PaceBand::Stale)
    } else if let Some(duration) = duration {
        let elapsed = duration.saturating_sub(remaining_ms);
        #[allow(clippy::cast_precision_loss)]
        let elapsed_percent = ((elapsed as f64 / duration as f64) * 100.0).max(1.0);
        let ratio = used / elapsed_percent;
        let band = if ratio > 1.5 && used > 50.0 {
            PaceBand::Conserve
        } else if ratio < 0.5 && remaining_ms < 3_600_000 {
            PaceBand::CapacityAvailable
        } else if ratio > 1.2 {
            PaceBand::SlightlyFast
        } else {
            PaceBand::OnTrack
        };
        (
            Some(round_tenth(elapsed_percent)),
            Some(round_tenth((ratio * 100.0).max(used))),
            band,
        )
    } else {
        (None, None, PaceBand::DurationUnavailable)
    };
    let chosen_machines = current
        .contributors
        .into_iter()
        .filter(|contributor| contributor.chosen)
        .map(|contributor| contributor.machine)
        .collect();
    UsagePace {
        profile: current.profile,
        window: current.window.kind,
        used_percent: current.window.used_percent,
        capacity_percent: round_tenth((100.0 - used).max(0.0)),
        remaining_ms,
        elapsed_percent,
        projected_used_percent,
        band,
        chosen_machines,
    }
}

const fn window_duration_ms(kind: QuotaWindowKind) -> Option<i64> {
    match kind {
        QuotaWindowKind::FiveHour => Some(5 * 60 * 60 * 1_000),
        QuotaWindowKind::RollingSevenDay | QuotaWindowKind::FixedWeekly => {
            Some(7 * 24 * 60 * 60 * 1_000)
        }
        QuotaWindowKind::MonthlyBudget => None,
    }
}

fn round_tenth(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextBand {
    Ok,
    Moderate,
    High,
    Critical,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextPace {
    pub session: ContextSession,
    pub band: ContextBand,
    pub tokens_until_compact: Option<u64>,
    pub tokens_available: Option<u64>,
}

/// Returns bounded per-session context capacity for one account.
///
/// # Errors
///
/// Returns store failures or excessive-session work.
pub async fn context_pace(
    store: &dyn Store,
    account_id: AccountId,
    profile: Option<ProfileName>,
) -> PulseResult<Vec<ContextPace>> {
    let sessions = store.list_context_sessions(account_id, profile).await?;
    if sessions.len() > MAX_CONTEXT_SESSIONS {
        return Err(PulseError::new(
            PulseErrorKind::Storage,
            "context pace exceeded its session work bound",
        ));
    }
    sessions.into_iter().map(context_for_session).collect()
}

fn context_for_session(session: ContextSession) -> PulseResult<ContextPace> {
    let band = match session.context_percent.map(Percent::get) {
        Some(percent) if percent >= 90.0 => ContextBand::Critical,
        Some(percent) if percent >= 75.0 => ContextBand::High,
        Some(percent) if percent >= 50.0 => ContextBand::Moderate,
        Some(_) => ContextBand::Ok,
        None => ContextBand::Unknown,
    };
    let tokens_available = session
        .effective_limit
        .zip(session.context_tokens)
        .map(|(limit, tokens)| limit.saturating_sub(tokens));
    let tokens_until_compact = session
        .effective_limit
        .zip(session.context_tokens)
        .map(|(limit, tokens)| {
            limit
                .checked_mul(75)
                .map(|threshold| threshold / 100)
                .ok_or_else(|| PulseError::invalid_input("context compact threshold overflowed"))
                .map(|threshold| threshold.saturating_sub(tokens))
        })
        .transpose()?;
    Ok(ContextPace {
        session,
        band,
        tokens_until_compact,
        tokens_available,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::pulse::{
        Account, AgentSettings, Machine, Profile, RefreshPolicy, SessionId, TokenSource, Vendor,
        store::SqliteStore,
    };

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    struct TestStore {
        path: PathBuf,
        store: SqliteStore,
    }

    impl TestStore {
        async fn new() -> Self {
            let id = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir()
                .join(format!("atmux-pulse-report-{}-{id}", std::process::id()));
            fs::create_dir(&directory).expect("private report test directory");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                    .expect("secure report test directory");
            }
            let path = directory.join("pulse.sqlite3");
            let store = SqliteStore::open(&path).await.expect("store");
            Self { path, store }
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            remove_sqlite_files(&self.path);
            if let Some(parent) = self.path.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }

    fn remove_sqlite_files(path: &PathBuf) {
        let _ = fs::remove_file(path);
        for suffix in ["-wal", "-shm"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            let _ = fs::remove_file(PathBuf::from(name));
        }
    }

    fn account(value: i64) -> AccountId {
        AccountId::new(value).expect("account")
    }

    fn profile_name(value: &str) -> ProfileName {
        ProfileName::new(value).expect("profile")
    }

    fn machine_name(value: &str) -> MachineName {
        MachineName::new(value).expect("machine")
    }

    fn instant(value: i64) -> Instant {
        Instant::from_epoch_millis(value).expect("instant")
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1.0e-9, "{actual} != {expected}");
    }

    async fn seed_identity(store: &SqliteStore, id: AccountId, identity: &str) {
        store
            .upsert_account(Account {
                id,
                identity: identity.to_owned(),
                display_name: None,
            })
            .await
            .expect("account");
        store
            .upsert_machine(Machine {
                account_id: id,
                name: machine_name("max"),
                first_seen: instant(1_000),
                last_seen: instant(2_000),
            })
            .await
            .expect("machine");
        store
            .upsert_profile(Profile {
                account_id: id,
                name: profile_name("claude"),
                vendor: Vendor::AnthropicOauth,
                config_dir: Some(PathBuf::from("/tmp/claude")),
                poll_interval_minutes: 15,
                monthly_budget_usd: None,
                api_key_env: None,
                api_key_file: None,
                refresh: RefreshPolicy::Never,
                hidden: false,
                origin: crate::pulse::ProfileOrigin::Local,
            })
            .await
            .expect("profile");
    }

    fn grain(account_id: AccountId, input: u64, day: &str) -> TokenGrain {
        let settings = AgentSettings::default();
        TokenGrain {
            account_id,
            profile: profile_name("claude"),
            machine: machine_name("max"),
            session_id: SessionId::new("session").expect("session"),
            model: "claude-opus-4-8".to_owned(),
            settings_hash: settings.sha256().expect("settings hash"),
            settings,
            day: day.to_owned(),
            tokens_in: input,
            tokens_out: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
            source: TokenSource::Local,
        }
    }

    fn request(account_id: AccountId) -> TokenReportRequest {
        TokenReportRequest {
            account_id,
            range: ReportRange {
                since_day: "2026-08-01".to_owned(),
                through_day: "2026-08-08".to_owned(),
            },
            granularity: ReportGranularity::Weekly,
            drill: ReportDrill::Model,
            profile: None,
            machine: None,
            exclude_profiles: BTreeSet::new(),
        }
    }

    #[tokio::test]
    async fn reports_are_account_scoped_weekly_and_repriced_at_read_time() {
        let database = TestStore::new().await;
        seed_identity(&database.store, account(1), "one@example.test").await;
        seed_identity(&database.store, account(2), "two@example.test").await;
        database
            .store
            .upsert_token_grain(grain(account(1), 1_000_000, "2026-08-08"))
            .await
            .expect("first grain");
        database
            .store
            .upsert_token_grain(grain(account(2), 9_000_000, "2026-08-08"))
            .await
            .expect("other account grain");

        let before = token_report(&database.store, request(account(1)))
            .await
            .expect("before");
        assert_eq!(before.total.tokens_in, 1_000_000);
        assert_close(before.total.cost_usd, 5.0);
        assert_eq!(before.profiles[0].by_period[0].day, "2026-08-03");

        database
            .store
            .upsert_pricing_override(
                account(1),
                PricingRule {
                    key: "opus-account-override".to_owned(),
                    vendor: Vendor::AnthropicOauth,
                    model_pattern: "claude-opus-4".to_owned(),
                    settings_match: BTreeMap::new(),
                    input_per_million_usd: 1.0,
                    output_per_million_usd: 0.0,
                    cache_write_5m_per_million_usd: 0.0,
                    cache_write_1h_per_million_usd: 0.0,
                    cache_read_per_million_usd: 0.0,
                },
            )
            .await
            .expect("override");
        let after = token_report(&database.store, request(account(1)))
            .await
            .expect("after");
        assert_close(after.total.cost_usd, 1.0);
        assert_eq!(after.total.tokens_in, 1_000_000);
    }

    #[test]
    fn report_ranges_and_work_are_bounded() {
        assert!(ReportRange::recent("2026-08-08", 0).is_err());
        assert!(ReportRange::recent("2026-08-08", 366).is_err());
        let invalid = ReportRange {
            since_day: "2025-01-01".to_owned(),
            through_day: "2026-08-08".to_owned(),
        };
        assert!(invalid.validate().is_err());
        let rows = (0..=MAX_REPORT_ROWS)
            .map(|index| grain(account(1), 1, &format!("2026-08-{:02}", index % 8 + 1)))
            .collect();
        let vendors = BTreeMap::from([(profile_name("claude"), Vendor::AnthropicOauth)]);
        assert!(
            build_report(request(account(1)), rows, &[], &[], &vendors).is_err(),
            "the in-memory work bound is independent of SQL behavior"
        );

        let bounded = build_report(
            request(account(1)),
            vec![
                grain(account(1), 9_000_000, "2026-07-31"),
                grain(account(1), 1_000_000, "2026-08-01"),
            ],
            &[],
            &[],
            &vendors,
        )
        .expect("bounded pure-store rows");
        assert_eq!(bounded.total.tokens_in, 1_000_000);
    }

    #[test]
    fn report_total_overflow_is_rejected() {
        let mut totals = ReportTotals::default();
        totals
            .add(&grain(account(1), u64::MAX, "2026-08-08"), 0.0)
            .expect("first add");
        assert!(
            totals
                .add(&grain(account(1), 1, "2026-08-08"), 0.0)
                .is_err()
        );
        let mut internally_overflowing = grain(account(1), u64::MAX, "2026-08-08");
        internally_overflowing.tokens_out = 1;
        assert!(
            ReportTotals::default()
                .add(&internally_overflowing, 0.0)
                .is_err()
        );
    }

    #[test]
    fn pace_classification_handles_stale_and_capacity() {
        let current = CurrentQuotaWindow {
            profile: profile_name("claude"),
            vendor: Vendor::AnthropicOauth,
            window: super::super::QuotaWindow {
                kind: QuotaWindowKind::FiveHour,
                used_percent: Percent::new(10.0).expect("percent"),
                resets_at: instant(4 * 60 * 60 * 1_000),
            },
            polled_at: instant(0),
            contributors: Vec::new(),
        };
        let pace = pace_for_window(current.clone(), instant(13_000_000));
        assert_eq!(pace.band, PaceBand::CapacityAvailable);
        let stale = pace_for_window(current, instant(5 * 60 * 60 * 1_000));
        assert_eq!(stale.band, PaceBand::Stale);
    }

    #[test]
    fn context_pace_exposes_compaction_and_capacity() {
        let session = ContextSession {
            account_id: account(1),
            profile: profile_name("claude"),
            machine: machine_name("max"),
            session_id: SessionId::new("session").expect("session"),
            model: Some("claude-opus-4-8".to_owned()),
            settings: AgentSettings::default(),
            context_tokens: Some(160_000),
            context_percent: Some(Percent::new(80.0).expect("percent")),
            effective_limit: Some(200_000),
            last_active_at: instant(2_000),
            last_reset_at: Some(instant(1_000)),
            collected_at: instant(3_000),
        };
        let pace = context_for_session(session).expect("context pace");
        assert_eq!(pace.band, ContextBand::High);
        assert_eq!(pace.tokens_until_compact, Some(0));
        assert_eq!(pace.tokens_available, Some(40_000));
    }
}
