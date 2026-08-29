//! Bounded account-scoped Pulse REST/MCP adapter.
//!
//! Authentication, host validation, mTLS, bearer tokens, and mutation Origin
//! checks remain owned by atmux's existing web policy. This module accepts an
//! explicit configured [`AccountId`] and never derives identity from headers.

use std::{collections::BTreeSet, convert::Infallible, fmt, pin::Pin, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{delete, get, patch, post},
};
use futures_core::Stream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    Account, AccountId, AlertDelivery, AlertSubscription, AlertType, Instant, MachineName, Percent,
    Profile, ProfileName, ProfileOrigin, PulseError, PulseErrorKind, PulseResult, RefreshPolicy,
    Vendor,
    health::{ProfileGaugeHealth, collect_gauge_health},
    ingest::{IngestTokenManager, IngestTokenSummary},
    invalidation::{PulseInvalidationHub, PulseInvalidationSubscription},
    model::{MAX_PROFILE_POLL_MINUTES, MIN_PROFILE_POLL_MINUTES},
    reports::{
        MAX_REPORT_DAYS, ReportDrill, ReportGranularity, ReportRange, TokenReportRequest,
        context_pace, token_report, usage_pace,
    },
    scheduler::ForcePollTarget,
    service::PulseManagement,
    store::{
        AlertReply, AlertReplyInput, CurrentQuotaWindow, MAX_ALERT_REPLY_BYTES, PricingRule, Store,
        StoredAlertSubscription, StoredUsageSnapshot,
    },
};

pub const MAX_PAGE_SIZE: usize = 100;
pub const DEFAULT_PAGE_SIZE: usize = 50;
pub const MAX_CURSOR_OFFSET: usize = 9_900;
pub const MAX_LIST_ROWS: usize = 10_000;
const MAX_ALERT_COOLDOWN_MINUTES: u32 = 7 * 24 * 60;
const MAX_PRICING_SETTINGS: usize = 32;
const MAX_PRICING_SETTING_BYTES: usize = 128;
const MAX_HEALTH_PROFILES: usize = 256;

#[derive(Clone)]
pub struct PulseApi {
    store: Arc<dyn Store>,
    accounts: Arc<BTreeSet<AccountId>>,
    capabilities: PulseCapabilities,
    local_machine: Option<MachineName>,
    management: Option<PulseManagement>,
    delivery_capabilities: PulseDeliveryCapabilities,
    invalidations: Option<PulseInvalidationHub>,
}

impl fmt::Debug for PulseApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PulseApi")
            .field("account_count", &self.accounts.len())
            .field("capabilities", &self.capabilities)
            .field("has_management", &self.management.is_some())
            .finish_non_exhaustive()
    }
}

impl PulseApi {
    #[must_use]
    pub fn new(
        store: Arc<dyn Store>,
        accounts: &[AccountId],
        capabilities: PulseCapabilities,
    ) -> Self {
        Self {
            store,
            accounts: Arc::new(accounts.iter().copied().collect()),
            capabilities,
            local_machine: None,
            management: None,
            delivery_capabilities: PulseDeliveryCapabilities::default(),
            invalidations: None,
        }
    }

    #[must_use]
    pub fn with_invalidations(mut self, invalidations: PulseInvalidationHub) -> Self {
        self.invalidations = Some(invalidations);
        self
    }

    /// Attaches command-only management and local delivery capabilities.
    #[must_use]
    pub fn with_management(
        mut self,
        local_machine: MachineName,
        management: Option<PulseManagement>,
        delivery_capabilities: PulseDeliveryCapabilities,
    ) -> Self {
        self.local_machine = Some(local_machine);
        self.management = management;
        self.delivery_capabilities = delivery_capabilities;
        self
    }

    fn account(&self, raw: i64) -> PulseResult<AccountId> {
        let account = AccountId::new(raw)?;
        if !self.accounts.contains(&account) {
            return Err(PulseError::new(
                PulseErrorKind::NotFound,
                "Pulse account was not found",
            ));
        }
        Ok(account)
    }

    fn subscribe_invalidations(
        &self,
        raw_account: i64,
        headers: &HeaderMap,
    ) -> PulseResult<PulseInvalidationSubscription> {
        let account_id = self.account(raw_account)?;
        parse_last_event_id(headers)?;
        self.invalidations
            .as_ref()
            .ok_or_else(|| {
                PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse invalidation stream is unavailable",
                )
            })?
            .subscribe(account_id)
    }

    fn publish(&self, account_id: AccountId) {
        if let Some(invalidations) = &self.invalidations {
            let _ = invalidations.publish(account_id);
        }
    }

    /// Lists the bounded accounts explicitly configured for this runtime.
    ///
    /// The outer atmux policy authenticates this endpoint. It never discovers
    /// arbitrary Store tenants and returns only secret-free account labels.
    ///
    /// # Errors
    ///
    /// Returns a storage error when configured bootstrap state is missing.
    pub async fn accounts(&self) -> PulseResult<Vec<Account>> {
        let mut accounts = Vec::with_capacity(self.accounts.len());
        for account_id in self.accounts.iter().copied() {
            let account = self.store.get_account(account_id).await?.ok_or_else(|| {
                PulseError::new(
                    PulseErrorKind::Storage,
                    "configured Pulse account is unavailable",
                )
            })?;
            accounts.push(account);
        }
        Ok(accounts)
    }

    async fn profiles_raw(&self, account: AccountId) -> PulseResult<Vec<Profile>> {
        bounded_rows(self.store.list_profiles(account).await?, "profiles")
    }

    async fn analytic_profiles(&self, account: AccountId) -> PulseResult<Vec<Profile>> {
        let profiles = self.profiles_raw(account).await?;
        if profiles.len() > super::reports::MAX_PACE_PROFILES {
            return Err(work_bound("analytic profiles"));
        }
        Ok(profiles)
    }

    /// Lists bounded account-global quota windows.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, work-bound, or store errors.
    pub async fn current_usage(
        &self,
        account: i64,
        profile: Option<String>,
        page: PageRequest,
    ) -> PulseResult<Page<CurrentQuotaWindow>> {
        let account = self.account(account)?;
        let profiles = if let Some(profile) = profile {
            let name = ProfileName::new(profile)?;
            let stored = self.store.get_profile(account, name.clone()).await?;
            if stored.is_none() {
                return Err(not_found("Pulse profile was not found"));
            }
            vec![name]
        } else {
            self.analytic_profiles(account)
                .await?
                .into_iter()
                .filter(|profile| !profile.hidden)
                .map(|profile| profile.name)
                .collect()
        };
        let mut rows = Vec::new();
        for profile in profiles {
            rows.extend(self.store.current_usage(account, profile).await?);
            if rows.len() > MAX_LIST_ROWS {
                return Err(work_bound("current usage"));
            }
        }
        paginate(rows, page)
    }

    /// Lists bounded usage history for one profile.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, pagination, or store errors.
    pub async fn history(
        &self,
        account: i64,
        profile: String,
        since: Option<String>,
        page: PageRequest,
    ) -> PulseResult<Page<StoredUsageSnapshot>> {
        let account = self.account(account)?;
        let profile = ProfileName::new(profile)?;
        if self
            .store
            .get_profile(account, profile.clone())
            .await?
            .is_none()
        {
            return Err(not_found("Pulse profile was not found"));
        }
        let since = since.as_deref().map(Instant::from_iso8601).transpose()?;
        let page = page.validate()?;
        let fetch = page
            .offset
            .saturating_add(page.limit)
            .saturating_add(1)
            .min(MAX_LIST_ROWS);
        let rows = self
            .store
            .usage_history(account, profile, since, fetch)
            .await?;
        paginate_validated(rows, page)
    }

    /// Computes bounded quota pace rows.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, report, or store errors.
    pub async fn pace(
        &self,
        account: i64,
        profile: Option<String>,
        page: PageRequest,
    ) -> PulseResult<Page<super::reports::UsagePace>> {
        let account = self.account(account)?;
        let profile = profile.map(ProfileName::new).transpose()?;
        paginate(
            usage_pace(self.store.as_ref(), account, profile, Instant::now()).await?,
            page,
        )
    }

    /// Computes bounded context capacity rows.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, report, or store errors.
    pub async fn context(
        &self,
        account: i64,
        profile: Option<String>,
        page: PageRequest,
    ) -> PulseResult<Page<super::reports::ContextPace>> {
        let account = self.account(account)?;
        let profile = profile.map(ProfileName::new).transpose()?;
        paginate(
            context_pace(self.store.as_ref(), account, profile).await?,
            page,
        )
    }

    /// Lists bounded Gemini quota buckets.
    ///
    /// # Errors
    ///
    /// Returns account-isolation, pagination, work-bound, or store errors.
    pub async fn gemini(
        &self,
        account: i64,
        page: PageRequest,
    ) -> PulseResult<Page<super::GeminiQuota>> {
        let account = self.account(account)?;
        paginate(
            bounded_rows(
                self.store.list_gemini_quotas(account).await?,
                "Gemini quotas",
            )?,
            page,
        )
    }

    /// Builds one bounded token and cost report.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, work-bound, pricing, or store errors.
    pub async fn report(
        &self,
        account: i64,
        query: ReportQuery,
    ) -> PulseResult<super::reports::TokenReport> {
        let account = self.account(account)?;
        let through = query.through_day.unwrap_or_else(today);
        let profile = query.profile.map(ProfileName::new).transpose()?;
        let machine = query.machine.map(MachineName::new).transpose()?;
        let exclude_profiles = if profile.is_some() {
            BTreeSet::new()
        } else {
            self.analytic_profiles(account)
                .await?
                .into_iter()
                .filter(|profile| profile.hidden)
                .map(|profile| profile.name)
                .collect()
        };
        token_report(
            self.store.as_ref(),
            TokenReportRequest {
                account_id: account,
                range: ReportRange::recent(&through, query.days.unwrap_or(30))?,
                granularity: query.granularity.unwrap_or_default(),
                drill: query.drill.unwrap_or_default(),
                profile,
                machine,
                exclude_profiles,
            },
        )
        .await
    }

    /// Lists secret-free profiles for one account.
    ///
    /// # Errors
    ///
    /// Returns account-isolation, pagination, work-bound, or store errors.
    pub async fn profiles(
        &self,
        account: i64,
        page: PageRequest,
    ) -> PulseResult<Page<PublicProfile>> {
        let account = self.account(account)?;
        let profiles = self
            .profiles_raw(account)
            .await?
            .into_iter()
            .map(PublicProfile::from)
            .collect();
        paginate(profiles, page)
    }

    /// Returns bounded, account-scoped gauge diagnostics for the local machine.
    ///
    /// # Errors
    ///
    /// Returns account-isolation, capability, work-bound, or store errors.
    pub async fn health(
        &self,
        account: i64,
        page: PageRequest,
    ) -> PulseResult<Page<ProfileGaugeHealth>> {
        let account = self.account(account)?;
        let machine = self.local_machine.as_ref().ok_or_else(|| {
            PulseError::new(
                PulseErrorKind::Conflict,
                "Pulse local health is unavailable",
            )
        })?;
        let profiles = self.profiles_raw(account).await?;
        if profiles.len() > MAX_HEALTH_PROFILES {
            return Err(work_bound("health profiles"));
        }
        paginate(
            collect_gauge_health(self.store.as_ref(), &profiles, machine, Instant::now()).await?,
            page,
        )
    }

    /// Lists bounded machine provenance for one account.
    ///
    /// # Errors
    ///
    /// Returns account-isolation, pagination, work-bound, or store errors.
    pub async fn machines(
        &self,
        account: i64,
        page: PageRequest,
    ) -> PulseResult<Page<super::Machine>> {
        let account = self.account(account)?;
        paginate(
            bounded_rows(self.store.list_machines(account).await?, "machines")?,
            page,
        )
    }

    /// Lists secret-free receiver token metadata for one configured account.
    ///
    /// # Errors
    ///
    /// Returns account-isolation, capability, pagination, or store errors.
    pub async fn ingest_tokens(
        &self,
        account: i64,
        page: PageRequest,
    ) -> PulseResult<Page<IngestTokenSummary>> {
        self.require_receiver()?;
        let account = self.account(account)?;
        paginate(
            bounded_rows(
                IngestTokenManager::new(Arc::clone(&self.store))
                    .list(account)
                    .await?,
                "ingest tokens",
            )?,
            page,
        )
    }

    /// Registers a validated remote machine and issues its one-time bearer.
    ///
    /// # Errors
    ///
    /// Returns account-isolation, capability, validation, conflict, randomness,
    /// or store errors. Plaintext is returned by this call only.
    pub async fn issue_ingest_token(
        &self,
        account: i64,
        machine: String,
    ) -> PulseResult<IssuedIngestTokenResponse> {
        self.require_receiver()?;
        let account = self.account(account)?;
        let machine = MachineName::new(machine)?;
        let issued = IngestTokenManager::new(Arc::clone(&self.store))
            .issue(account, machine, Instant::now())
            .await?;
        self.publish(account);
        let (summary, token) = issued.into_parts();
        Ok(IssuedIngestTokenResponse { summary, token })
    }

    /// Revokes one account-scoped receiver token.
    ///
    /// # Errors
    ///
    /// Returns account-isolation, capability, validation, not-found, or store
    /// errors. Cross-account ids use the same not-found envelope.
    pub async fn revoke_ingest_token(&self, account: i64, token_id: i64) -> PulseResult<()> {
        self.require_receiver()?;
        let account = self.account(account)?;
        positive_id(token_id, "ingest token id")?;
        if !IngestTokenManager::new(Arc::clone(&self.store))
            .revoke(account, token_id, Instant::now())
            .await?
        {
            return Err(not_found("Pulse ingest token was not found"));
        }
        self.publish(account);
        Ok(())
    }

    fn require_receiver(&self) -> PulseResult<()> {
        if !self.capabilities.receive {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "Pulse receiver is disabled",
            ));
        }
        Ok(())
    }

    /// Lists bounded alert events for one account.
    ///
    /// # Errors
    ///
    /// Returns account-isolation, pagination, work-bound, or store errors.
    pub async fn alerts(
        &self,
        account: i64,
        acknowledged: Option<bool>,
        page: PageRequest,
    ) -> PulseResult<Page<super::store::AlertEvent>> {
        let account = self.account(account)?;
        paginate(
            bounded_rows(
                self.store.list_alert_events(account, acknowledged).await?,
                "alert events",
            )?,
            page,
        )
    }

    /// Lists bounded alert subscriptions for one account.
    ///
    /// # Errors
    ///
    /// Returns account-isolation, pagination, work-bound, or store errors.
    pub async fn subscriptions(
        &self,
        account: i64,
        page: PageRequest,
    ) -> PulseResult<Page<StoredAlertSubscription>> {
        let account = self.account(account)?;
        paginate(
            bounded_rows(
                self.store.list_alert_subscriptions(account).await?,
                "alert subscriptions",
            )?,
            page,
        )
    }

    /// Lists bounded replies for an account-scoped alert.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, pagination, or store errors.
    pub async fn alert_replies(
        &self,
        account: i64,
        alert_id: i64,
        page: PageRequest,
    ) -> PulseResult<Page<AlertReply>> {
        let account = self.account(account)?;
        positive_id(alert_id, "alert id")?;
        paginate(
            bounded_rows(
                self.store.list_alert_replies(account, alert_id).await?,
                "alert replies",
            )?,
            page,
        )
    }

    /// Lists bounded effective pricing rules without secret material.
    ///
    /// # Errors
    ///
    /// Returns account-isolation, pagination, work-bound, or store errors.
    pub async fn pricing(
        &self,
        account: i64,
        page: PageRequest,
    ) -> PulseResult<Page<PublicPricingRule>> {
        let account = self.account(account)?;
        let mut rows = bounded_rows(self.store.list_pricing_defaults().await?, "default pricing")?
            .into_iter()
            .map(|rule| PublicPricingRule {
                scope: PricingScope::Default,
                rule,
            })
            .collect::<Vec<_>>();
        rows.extend(
            bounded_rows(
                self.store.list_pricing_overrides(account).await?,
                "pricing overrides",
            )?
            .into_iter()
            .map(|rule| PublicPricingRule {
                scope: PricingScope::Override,
                rule,
            }),
        );
        paginate(rows, page)
    }

    /// Returns server-enforced API bounds and capabilities.
    ///
    /// # Errors
    ///
    /// Returns not-found when the explicit account is not configured.
    pub fn limits(&self, account: i64) -> PulseResult<PulseLimits> {
        self.account(account)?;
        Ok(PulseLimits {
            max_page_size: MAX_PAGE_SIZE,
            max_cursor_offset: MAX_CURSOR_OFFSET,
            max_report_days: MAX_REPORT_DAYS,
            max_request_body_bytes: crate::MAX_REQUEST_BODY_BYTES,
            max_alert_reply_bytes: MAX_ALERT_REPLY_BYTES,
            min_profile_poll_minutes: MIN_PROFILE_POLL_MINUTES,
            max_profile_poll_minutes: MAX_PROFILE_POLL_MINUTES,
            force_poll_available: self.management.is_some() && self.capabilities.collect,
            capabilities: self.capabilities,
            delivery: self.delivery_capabilities,
        })
    }

    /// Changes one account-scoped profile's visibility.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, not-found, or store errors.
    pub async fn set_visibility(
        &self,
        account: i64,
        profile: String,
        hidden: bool,
    ) -> PulseResult<()> {
        let account = self.account(account)?;
        let profile = ProfileName::new(profile)?;
        if !self
            .store
            .set_profile_hidden(account, profile, hidden)
            .await?
        {
            return Err(not_found("Pulse profile was not found"));
        }
        self.publish(account);
        Ok(())
    }

    /// Changes only bounded, non-secret profile settings while retaining all
    /// credential references and local paths server-side.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, not-found, or store errors.
    pub async fn update_profile_settings(
        &self,
        account: i64,
        profile: String,
        input: ProfileSettingsInput,
    ) -> PulseResult<()> {
        input.validate()?;
        let account = self.account(account)?;
        let profile = ProfileName::new(profile)?;
        let mut stored = self
            .store
            .get_profile(account, profile)
            .await?
            .ok_or_else(|| not_found("Pulse profile was not found"))?;
        if let Some(minutes) = input.poll_interval_minutes {
            stored.poll_interval_minutes = minutes;
        }
        match input.monthly_budget_usd {
            MonthlyBudgetPatch::Missing => {}
            MonthlyBudgetPatch::Clear => stored.monthly_budget_usd = None,
            MonthlyBudgetPatch::Set(budget) => stored.monthly_budget_usd = Some(budget),
        }
        stored.validate().map_err(|_| {
            PulseError::invalid_input("profile settings are incompatible with this profile")
        })?;
        self.store.upsert_profile(stored).await?;
        self.publish(account);
        Ok(())
    }

    /// Coalesces one bounded account- or profile-scoped collection pass through
    /// the sole running scheduler.
    ///
    /// # Errors
    ///
    /// Returns account-isolation, capability, queue, or shutdown errors.
    pub async fn force_poll(
        &self,
        account: i64,
        profile: Option<String>,
    ) -> PulseResult<ForcePollResponse> {
        let account = self.account(account)?;
        let target = if let Some(profile) = profile {
            let profile = ProfileName::new(profile)?;
            let stored = self
                .store
                .get_profile(account, profile.clone())
                .await?
                .ok_or_else(|| not_found("Pulse profile was not found"))?;
            if stored.origin != ProfileOrigin::Local {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse profile is not locally collectable",
                ));
            }
            ForcePollTarget::profile(account, profile)
        } else {
            ForcePollTarget::account(account)
        };
        let management = self.management.as_ref().ok_or_else(|| {
            PulseError::new(PulseErrorKind::Conflict, "Pulse collection is not enabled")
        })?;
        management.force_poll(target)?;
        self.publish(account);
        Ok(ForcePollResponse { queued: true })
    }

    /// Acknowledges one account-scoped alert. This is deliberately explicit-id
    /// only: callers page alerts and confirm each bounded mutation instead of
    /// issuing an unbounded account-wide acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, not-found, or store errors.
    pub async fn acknowledge_alert(&self, account: i64, alert_id: i64) -> PulseResult<()> {
        let account = self.account(account)?;
        positive_id(alert_id, "alert id")?;
        if !self.store.acknowledge_alert(account, alert_id).await? {
            return Err(not_found("Pulse alert was not found"));
        }
        self.publish(account);
        Ok(())
    }

    /// Persists a bounded operator reply and acknowledges its alert atomically.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, not-found, or store errors.
    pub async fn reply_alert(
        &self,
        account: i64,
        alert_id: i64,
        message: String,
    ) -> PulseResult<ReplyResult> {
        validate_reply(&message)?;
        let account = self.account(account)?;
        positive_id(alert_id, "alert id")?;
        let reply = self
            .store
            .reply_to_alert(AlertReplyInput {
                account_id: account,
                event_id: alert_id,
                message,
                replied_at: Instant::now(),
            })
            .await?
            .ok_or_else(|| not_found("Pulse alert was not found"))?;
        self.publish(account);
        Ok(ReplyResult {
            id: reply.id,
            event_id: reply.event_id,
            acknowledged: true,
            persisted: true,
            replied_at: reply.replied_at,
        })
    }

    /// Creates an account-scoped alert subscription.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, not-found, conflict, or store errors.
    pub async fn create_subscription(
        &self,
        account: i64,
        input: AlertSubscriptionInput,
    ) -> PulseResult<StoredAlertSubscription> {
        if input.cooldown_minutes > MAX_ALERT_COOLDOWN_MINUTES {
            return Err(PulseError::invalid_input(
                "alert cooldown cannot exceed seven days",
            ));
        }
        let account = self.account(account)?;
        let profile = ProfileName::new(input.profile)?;
        if self
            .store
            .get_profile(account, profile.clone())
            .await?
            .is_none()
        {
            return Err(not_found("Pulse profile was not found"));
        }
        let subscription = self
            .store
            .create_alert_subscription(
                AlertSubscription {
                    account_id: account,
                    profile,
                    alert_type: input.alert_type,
                    threshold: input.threshold,
                    cooldown_minutes: input.cooldown_minutes,
                    delivery: input.delivery,
                    enabled: input.enabled,
                },
                Instant::now(),
            )
            .await?;
        self.publish(account);
        Ok(subscription)
    }

    /// Deletes an account-scoped alert subscription.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, not-found, or store errors.
    pub async fn delete_subscription(&self, account: i64, subscription_id: i64) -> PulseResult<()> {
        let account = self.account(account)?;
        positive_id(subscription_id, "subscription id")?;
        if !self
            .store
            .delete_alert_subscription(account, subscription_id)
            .await?
        {
            return Err(not_found("Pulse alert subscription was not found"));
        }
        self.publish(account);
        Ok(())
    }

    /// Upserts one validated account pricing override.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, or store errors.
    pub async fn upsert_pricing(&self, account: i64, input: PricingRuleInput) -> PulseResult<()> {
        let account = self.account(account)?;
        self.store
            .upsert_pricing_override(account, input.into_rule()?)
            .await?;
        self.publish(account);
        Ok(())
    }

    /// Deletes one account override so the authoritative seeded default (if
    /// any) becomes effective again.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, indistinguishable not-found, or
    /// store errors. Seeded defaults are never mutated by this operation.
    pub async fn delete_pricing_override(&self, account: i64, key: String) -> PulseResult<()> {
        let account = self.account(account)?;
        super::store::validate_pricing_key(&key)?;
        if !self.store.delete_pricing_override(account, key).await? {
            return Err(not_found("Pulse pricing override was not found"));
        }
        self.publish(account);
        Ok(())
    }

    /// Executes the bounded MCP read contract with an explicit account id.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, serialization, or store errors.
    pub async fn read_mcp(&self, request: PulseMcpReadRequest) -> PulseResult<serde_json::Value> {
        let page = PageRequest {
            cursor: request.cursor,
            limit: request.limit,
        };
        let value = match request.resource {
            PulseReadResource::CurrentUsage => serde_json::to_value(
                self.current_usage(request.account_id, request.profile, page)
                    .await?,
            ),
            PulseReadResource::History => serde_json::to_value(
                self.history(
                    request.account_id,
                    required(request.profile, "history requires profile")?,
                    request.since,
                    page,
                )
                .await?,
            ),
            PulseReadResource::Pace => {
                serde_json::to_value(self.pace(request.account_id, request.profile, page).await?)
            }
            PulseReadResource::Context => serde_json::to_value(
                self.context(request.account_id, request.profile, page)
                    .await?,
            ),
            PulseReadResource::Gemini => {
                serde_json::to_value(self.gemini(request.account_id, page).await?)
            }
            PulseReadResource::Report => serde_json::to_value(
                self.report(
                    request.account_id,
                    ReportQuery {
                        through_day: request.through_day,
                        days: request.days,
                        granularity: request.granularity,
                        drill: request.drill,
                        profile: request.profile,
                        machine: request.machine,
                    },
                )
                .await?,
            ),
            PulseReadResource::Profiles => {
                serde_json::to_value(self.profiles(request.account_id, page).await?)
            }
            PulseReadResource::Health => {
                serde_json::to_value(self.health(request.account_id, page).await?)
            }
            PulseReadResource::Alerts => serde_json::to_value(
                self.alerts(request.account_id, request.acknowledged, page)
                    .await?,
            ),
            PulseReadResource::Subscriptions => {
                serde_json::to_value(self.subscriptions(request.account_id, page).await?)
            }
            PulseReadResource::AlertReplies => serde_json::to_value(
                self.alert_replies(
                    request.account_id,
                    required(request.alert_id, "alert replies require alert_id")?,
                    page,
                )
                .await?,
            ),
            PulseReadResource::Pricing => {
                serde_json::to_value(self.pricing(request.account_id, page).await?)
            }
            PulseReadResource::Limits => serde_json::to_value(self.limits(request.account_id)?),
            PulseReadResource::Machines => {
                serde_json::to_value(self.machines(request.account_id, page).await?)
            }
            PulseReadResource::IngestTokens => {
                serde_json::to_value(self.ingest_tokens(request.account_id, page).await?)
            }
        };
        value.map_err(|_| PulseError::new(PulseErrorKind::Internal, "Pulse response failed"))
    }

    /// Executes the bounded MCP mutation contract with an explicit account id.
    ///
    /// # Errors
    ///
    /// Returns validation, account-isolation, not-found, conflict, or store errors.
    pub async fn mutate_mcp(
        &self,
        request: PulseMcpMutationRequest,
    ) -> PulseResult<serde_json::Value> {
        let value =
            match request.action {
                PulseMutationAction::ProfileVisibility => {
                    self.set_visibility(
                        request.account_id,
                        required(request.profile, "visibility requires profile")?,
                        required(request.hidden, "visibility requires hidden")?,
                    )
                    .await?;
                    ok_json()
                }
                PulseMutationAction::ProfileSettings => {
                    self.update_profile_settings(
                        request.account_id,
                        required(request.profile, "profile settings require profile")?,
                        required(
                            request.profile_settings,
                            "profile settings require settings",
                        )?,
                    )
                    .await?;
                    ok_json()
                }
                PulseMutationAction::ForcePoll => serde_json::to_value(
                    self.force_poll(request.account_id, request.profile).await?,
                )
                .map_err(|_| PulseError::new(PulseErrorKind::Internal, "Pulse response failed"))?,
                PulseMutationAction::AlertAcknowledge => {
                    self.acknowledge_alert(
                        request.account_id,
                        required(request.alert_id, "acknowledge requires alert_id")?,
                    )
                    .await?;
                    ok_json()
                }
                PulseMutationAction::AlertReply => serde_json::to_value(
                    self.reply_alert(
                        request.account_id,
                        required(request.alert_id, "reply requires alert_id")?,
                        required(request.message, "reply requires message")?,
                    )
                    .await?,
                )
                .map_err(|_| PulseError::new(PulseErrorKind::Internal, "Pulse response failed"))?,
                PulseMutationAction::SubscriptionCreate => serde_json::to_value(
                    self.create_subscription(
                        request.account_id,
                        required(request.subscription, "create requires subscription")?,
                    )
                    .await?,
                )
                .map_err(|_| PulseError::new(PulseErrorKind::Internal, "Pulse response failed"))?,
                PulseMutationAction::SubscriptionDelete => {
                    self.delete_subscription(
                        request.account_id,
                        required(request.subscription_id, "delete requires subscription_id")?,
                    )
                    .await?;
                    ok_json()
                }
                PulseMutationAction::PricingUpsert => {
                    self.upsert_pricing(
                        request.account_id,
                        required(request.pricing, "pricing upsert requires pricing")?,
                    )
                    .await?;
                    ok_json()
                }
                PulseMutationAction::PricingDelete => {
                    self.delete_pricing_override(
                        request.account_id,
                        required(request.pricing_key, "pricing delete requires pricing_key")?,
                    )
                    .await?;
                    ok_json()
                }
                PulseMutationAction::IngestTokenIssue => serde_json::to_value(
                    self.issue_ingest_token(
                        request.account_id,
                        required(request.machine, "ingest token issue requires machine")?,
                    )
                    .await?,
                )
                .map_err(|_| PulseError::new(PulseErrorKind::Internal, "Pulse response failed"))?,
                PulseMutationAction::IngestTokenRevoke => {
                    self.revoke_ingest_token(
                        request.account_id,
                        required(request.token_id, "ingest token revoke requires token_id")?,
                    )
                    .await?;
                    ok_json()
                }
            };
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PulseCapabilities {
    pub collect: bool,
    pub serve: bool,
    pub receive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PulseDeliveryCapabilities {
    pub pull: bool,
    pub pane: bool,
    pub channel: bool,
}

impl Default for PulseDeliveryCapabilities {
    fn default() -> Self {
        Self {
            pull: true,
            pane: false,
            channel: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct PageRequest {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
struct ValidatedPage {
    offset: usize,
    limit: usize,
}

impl PageRequest {
    fn validate(self) -> PulseResult<ValidatedPage> {
        let offset = self.cursor.as_deref().map_or(Ok(0), parse_cursor)?;
        let limit = self.limit.unwrap_or(DEFAULT_PAGE_SIZE);
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(PulseError::invalid_input(format!(
                "page limit must be between 1 and {MAX_PAGE_SIZE}"
            )));
        }
        Ok(ValidatedPage { offset, limit })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublicProfile {
    pub account_id: AccountId,
    pub name: ProfileName,
    pub vendor: Vendor,
    pub poll_interval_minutes: u32,
    pub monthly_budget_usd: Option<f64>,
    pub refresh: RefreshPolicy,
    pub hidden: bool,
    pub origin: ProfileOrigin,
    pub has_config_dir: bool,
    pub credential_source: Option<CredentialSource>,
}

impl From<Profile> for PublicProfile {
    fn from(profile: Profile) -> Self {
        let credential_source = if profile.api_key_env.is_some() {
            Some(CredentialSource::Environment)
        } else if profile.api_key_file.is_some() {
            Some(CredentialSource::File)
        } else {
            None
        };
        Self {
            account_id: profile.account_id,
            name: profile.name,
            vendor: profile.vendor,
            poll_interval_minutes: profile.poll_interval_minutes,
            monthly_budget_usd: profile.monthly_budget_usd,
            refresh: profile.refresh,
            hidden: profile.hidden,
            origin: profile.origin,
            has_config_dir: profile.config_dir.is_some(),
            credential_source,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    Environment,
    File,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublicPricingRule {
    pub scope: PricingScope,
    #[serde(flatten)]
    pub rule: PricingRule,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingScope {
    Default,
    Override,
}

/// Durable reply acknowledgement that deliberately does not echo the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyResult {
    pub id: i64,
    pub event_id: i64,
    pub acknowledged: bool,
    pub persisted: bool,
    pub replied_at: Instant,
}

/// One-time receiver credential response. The token is deliberately absent
/// from every list/read model and from all `Debug` output.
#[derive(Serialize)]
pub struct IssuedIngestTokenResponse {
    pub summary: IngestTokenSummary,
    pub token: String,
}

impl fmt::Debug for IssuedIngestTokenResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedIngestTokenResponse")
            .field("summary", &self.summary)
            .field("token", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PulseLimits {
    pub max_page_size: usize,
    pub max_cursor_offset: usize,
    pub max_report_days: u16,
    pub max_request_body_bytes: usize,
    pub max_alert_reply_bytes: usize,
    pub min_profile_poll_minutes: u32,
    pub max_profile_poll_minutes: u32,
    pub force_poll_available: bool,
    pub capabilities: PulseCapabilities,
    pub delivery: PulseDeliveryCapabilities,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForcePollResponse {
    pub queued: bool,
}

type PulseEventStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

fn parse_last_event_id(headers: &HeaderMap) -> PulseResult<Option<u64>> {
    let Some(value) = headers.get("last-event-id") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| PulseError::invalid_input("Last-Event-ID must be an unsigned integer"))?;
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PulseError::invalid_input(
            "Last-Event-ID must be an unsigned integer",
        ));
    }
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| PulseError::invalid_input("Last-Event-ID is out of range"))
}

fn pulse_event(revision: u64) -> Event {
    Event::default()
        .event("pulse")
        .id(revision.to_string())
        .data(format!(r#"{{"revision":{revision}}}"#))
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForcePollInput {
    /// Omit for every local profile in the account.
    pub profile: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct AlertSubscriptionInput {
    pub profile: String,
    pub alert_type: AlertType,
    pub threshold: Option<Percent>,
    #[serde(default = "default_cooldown")]
    pub cooldown_minutes: u32,
    pub delivery: Option<AlertDelivery>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileSettingsInput {
    pub poll_interval_minutes: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_budget_patch")]
    #[schemars(with = "Option<f64>")]
    pub monthly_budget_usd: MonthlyBudgetPatch,
}

/// Tri-state monthly-budget mutation. JSON omission preserves the existing
/// value, explicit `null` clears it, and a number replaces it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum MonthlyBudgetPatch {
    #[default]
    Missing,
    Clear,
    Set(f64),
}

impl ProfileSettingsInput {
    fn validate(self) -> PulseResult<()> {
        if self.poll_interval_minutes.is_none()
            && self.monthly_budget_usd == MonthlyBudgetPatch::Missing
        {
            return Err(PulseError::invalid_input(
                "profile settings require poll_interval_minutes or monthly_budget_usd",
            ));
        }
        if self.poll_interval_minutes.is_some_and(|minutes| {
            !(MIN_PROFILE_POLL_MINUTES..=MAX_PROFILE_POLL_MINUTES).contains(&minutes)
        }) {
            return Err(PulseError::invalid_input(format!(
                "profile poll interval must be between {MIN_PROFILE_POLL_MINUTES} and {MAX_PROFILE_POLL_MINUTES} minutes"
            )));
        }
        if let MonthlyBudgetPatch::Set(budget) = self.monthly_budget_usd
            && (!budget.is_finite() || budget <= 0.0 || budget > 1_000_000.0)
        {
            return Err(PulseError::invalid_input(
                "monthly budget must be greater than zero and at most 1000000 USD",
            ));
        }
        Ok(())
    }
}

fn deserialize_budget_patch<'de, D>(deserializer: D) -> Result<MonthlyBudgetPatch, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<f64>::deserialize(deserializer).map(|budget| match budget {
        Some(budget) => MonthlyBudgetPatch::Set(budget),
        None => MonthlyBudgetPatch::Clear,
    })
}

const fn default_cooldown() -> u32 {
    30
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct PricingRuleInput {
    pub key: String,
    pub vendor: Vendor,
    pub model_pattern: String,
    #[serde(default)]
    pub settings_match: std::collections::BTreeMap<String, String>,
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
    #[serde(default)]
    pub cache_write_5m_per_million_usd: f64,
    #[serde(default)]
    pub cache_write_1h_per_million_usd: f64,
    #[serde(default)]
    pub cache_read_per_million_usd: f64,
}

impl PricingRuleInput {
    fn into_rule(self) -> PulseResult<PricingRule> {
        if self.settings_match.len() > MAX_PRICING_SETTINGS
            || self.settings_match.iter().any(|(key, value)| {
                key.is_empty()
                    || key.len() > MAX_PRICING_SETTING_BYTES
                    || value.len() > MAX_PRICING_SETTING_BYTES
                    || key.trim() != key
                    || value.trim() != value
                    || key.chars().any(char::is_control)
                    || value.chars().any(char::is_control)
            })
        {
            return Err(PulseError::invalid_input(
                "pricing settings must contain at most 32 bounded text entries",
            ));
        }
        let rule = PricingRule {
            key: self.key,
            vendor: self.vendor,
            model_pattern: self.model_pattern,
            settings_match: self.settings_match,
            input_per_million_usd: self.input_per_million_usd,
            output_per_million_usd: self.output_per_million_usd,
            cache_write_5m_per_million_usd: self.cache_write_5m_per_million_usd,
            cache_write_1h_per_million_usd: self.cache_write_1h_per_million_usd,
            cache_read_per_million_usd: self.cache_read_per_million_usd,
        };
        rule.validate()?;
        Ok(rule)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct UsageQuery {
    pub profile: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct HistoryQuery {
    pub profile: String,
    pub since: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AlertQuery {
    pub acknowledged: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

fn page_request(cursor: Option<String>, limit: Option<usize>) -> PageRequest {
    PageRequest { cursor, limit }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct ReportQuery {
    pub through_day: Option<String>,
    pub days: Option<u16>,
    pub granularity: Option<ReportGranularity>,
    pub drill: Option<ReportDrill>,
    pub profile: Option<String>,
    pub machine: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct VisibilityBody {
    pub hidden: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReplyBody {
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PulseReadResource {
    CurrentUsage,
    History,
    Pace,
    Context,
    Gemini,
    Report,
    Profiles,
    Health,
    Alerts,
    Subscriptions,
    AlertReplies,
    Pricing,
    Limits,
    Machines,
    IngestTokens,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct PulseMcpReadRequest {
    /// Explicit configured Pulse account id. Headers are never used as identity.
    pub account_id: i64,
    pub resource: PulseReadResource,
    pub profile: Option<String>,
    pub machine: Option<String>,
    pub since: Option<String>,
    pub through_day: Option<String>,
    pub days: Option<u16>,
    pub granularity: Option<ReportGranularity>,
    pub drill: Option<ReportDrill>,
    pub acknowledged: Option<bool>,
    pub alert_id: Option<i64>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PulseMutationAction {
    ProfileVisibility,
    ProfileSettings,
    ForcePoll,
    AlertAcknowledge,
    AlertReply,
    SubscriptionCreate,
    SubscriptionDelete,
    PricingUpsert,
    PricingDelete,
    IngestTokenIssue,
    IngestTokenRevoke,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct PulseMcpMutationRequest {
    /// Explicit configured Pulse account id. Headers are never used as identity.
    pub account_id: i64,
    pub action: PulseMutationAction,
    pub profile: Option<String>,
    pub hidden: Option<bool>,
    pub profile_settings: Option<ProfileSettingsInput>,
    pub alert_id: Option<i64>,
    pub subscription_id: Option<i64>,
    pub message: Option<String>,
    pub subscription: Option<AlertSubscriptionInput>,
    pub pricing: Option<PricingRuleInput>,
    pub pricing_key: Option<String>,
    pub machine: Option<String>,
    pub token_id: Option<i64>,
}

#[derive(Debug)]
pub struct PulseHttpError(PulseError);

impl From<PulseError> for PulseHttpError {
    fn from(error: PulseError) -> Self {
        Self(error)
    }
}

impl IntoResponse for PulseHttpError {
    fn into_response(self) -> Response {
        let status = match self.0.kind() {
            PulseErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
            PulseErrorKind::NotFound => StatusCode::NOT_FOUND,
            PulseErrorKind::Conflict => StatusCode::CONFLICT,
            PulseErrorKind::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            PulseErrorKind::Offline => StatusCode::SERVICE_UNAVAILABLE,
            PulseErrorKind::Authentication | PulseErrorKind::Upstream => StatusCode::BAD_GATEWAY,
            PulseErrorKind::Storage | PulseErrorKind::Configuration | PulseErrorKind::Internal => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (
            status,
            Json(serde_json::json!({
                "error": self.0.message(),
                "kind": self.0.kind(),
            })),
        )
            .into_response()
    }
}

/// REST routes. The caller must wrap this router in atmux's existing host,
/// token/mTLS, cache, body-size, and mutation-Origin policies.
pub fn router(api: PulseApi) -> Router {
    Router::new()
        .route("/api/v1/pulse/accounts", get(rest_accounts))
        .route("/api/v1/pulse/accounts/{account}/usage", get(rest_usage))
        .route(
            "/api/v1/pulse/accounts/{account}/history",
            get(rest_history),
        )
        .route("/api/v1/pulse/accounts/{account}/pace", get(rest_pace))
        .route(
            "/api/v1/pulse/accounts/{account}/context",
            get(rest_context),
        )
        .route("/api/v1/pulse/accounts/{account}/gemini", get(rest_gemini))
        .route("/api/v1/pulse/accounts/{account}/reports", get(rest_report))
        .route(
            "/api/v1/pulse/accounts/{account}/profiles",
            get(rest_profiles),
        )
        .route(
            "/api/v1/pulse/accounts/{account}/profiles/{profile}/visibility",
            patch(rest_visibility),
        )
        .route(
            "/api/v1/pulse/accounts/{account}/profiles/{profile}/settings",
            patch(rest_profile_settings),
        )
        .route("/api/v1/pulse/accounts/{account}/health", get(rest_health))
        .route("/api/v1/pulse/accounts/{account}/events", get(rest_events))
        .route(
            "/api/v1/pulse/accounts/{account}/poll",
            post(rest_force_poll),
        )
        .route("/api/v1/pulse/accounts/{account}/alerts", get(rest_alerts))
        .route(
            "/api/v1/pulse/accounts/{account}/alerts/{alert_id}/acknowledge",
            post(rest_acknowledge),
        )
        .route(
            "/api/v1/pulse/accounts/{account}/alerts/{alert_id}/reply",
            get(rest_alert_replies).post(rest_reply),
        )
        .route(
            "/api/v1/pulse/accounts/{account}/alert-subscriptions",
            get(rest_subscriptions).post(rest_create_subscription),
        )
        .route(
            "/api/v1/pulse/accounts/{account}/alert-subscriptions/{subscription_id}",
            delete(rest_delete_subscription),
        )
        .route(
            "/api/v1/pulse/accounts/{account}/pricing",
            get(rest_pricing).post(rest_upsert_pricing),
        )
        .route(
            "/api/v1/pulse/accounts/{account}/pricing/{key}",
            delete(rest_delete_pricing),
        )
        .route("/api/v1/pulse/accounts/{account}/limits", get(rest_limits))
        .route(
            "/api/v1/pulse/accounts/{account}/machines",
            get(rest_machines),
        )
        .route(
            "/api/v1/pulse/accounts/{account}/ingest-tokens",
            get(rest_ingest_tokens).post(rest_issue_ingest_token),
        )
        .route(
            "/api/v1/pulse/accounts/{account}/ingest-tokens/{token_id}",
            delete(rest_revoke_ingest_token),
        )
        .with_state(api)
}

async fn rest_accounts(State(api): State<PulseApi>) -> Result<Json<Vec<Account>>, PulseHttpError> {
    Ok(Json(api.accounts().await?))
}

async fn rest_events(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, PulseHttpError> {
    let subscription = api.subscribe_invalidations(account, &headers)?;
    let initial_revision = subscription.revision;
    let mut receiver = subscription.receiver;
    let stream = async_stream::stream! {
        yield Ok(pulse_event(initial_revision));
        while receiver.changed().await.is_ok() {
            let revision = *receiver.borrow_and_update();
            yield Ok(pulse_event(revision));
        }
    };
    Ok(Sse::new(Box::pin(stream) as PulseEventStream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(20))
                .text("pulse"),
        )
        .into_response())
}

async fn rest_usage(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Page<CurrentQuotaWindow>>, PulseHttpError> {
    api.current_usage(
        account,
        query.profile,
        page_request(query.cursor, query.limit),
    )
    .await
    .map(Json)
    .map_err(Into::into)
}

async fn rest_history(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Page<StoredUsageSnapshot>>, PulseHttpError> {
    api.history(
        account,
        query.profile,
        query.since,
        page_request(query.cursor, query.limit),
    )
    .await
    .map(Json)
    .map_err(Into::into)
}

async fn rest_pace(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Page<super::reports::UsagePace>>, PulseHttpError> {
    api.pace(
        account,
        query.profile,
        page_request(query.cursor, query.limit),
    )
    .await
    .map(Json)
    .map_err(Into::into)
}

async fn rest_context(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Page<super::reports::ContextPace>>, PulseHttpError> {
    api.context(
        account,
        query.profile,
        page_request(query.cursor, query.limit),
    )
    .await
    .map(Json)
    .map_err(Into::into)
}

async fn rest_gemini(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(page): Query<PageRequest>,
) -> Result<Json<Page<super::GeminiQuota>>, PulseHttpError> {
    api.gemini(account, page)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn rest_report(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(query): Query<ReportQuery>,
) -> Result<Json<super::reports::TokenReport>, PulseHttpError> {
    api.report(account, query)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn rest_profiles(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(page): Query<PageRequest>,
) -> Result<Json<Page<PublicProfile>>, PulseHttpError> {
    api.profiles(account, page)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn rest_visibility(
    State(api): State<PulseApi>,
    Path((account, profile)): Path<(i64, String)>,
    Json(body): Json<VisibilityBody>,
) -> Result<StatusCode, PulseHttpError> {
    api.set_visibility(account, profile, body.hidden)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Into::into)
}

async fn rest_profile_settings(
    State(api): State<PulseApi>,
    Path((account, profile)): Path<(i64, String)>,
    Json(body): Json<ProfileSettingsInput>,
) -> Result<StatusCode, PulseHttpError> {
    api.update_profile_settings(account, profile, body)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Into::into)
}

async fn rest_health(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(page): Query<PageRequest>,
) -> Result<Json<Page<ProfileGaugeHealth>>, PulseHttpError> {
    api.health(account, page)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn rest_force_poll(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    body: Option<Json<ForcePollInput>>,
) -> Result<(StatusCode, Json<ForcePollResponse>), PulseHttpError> {
    api.force_poll(account, body.and_then(|Json(body)| body.profile))
        .await
        .map(|response| (StatusCode::ACCEPTED, Json(response)))
        .map_err(Into::into)
}

async fn rest_alerts(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(query): Query<AlertQuery>,
) -> Result<Json<Page<super::store::AlertEvent>>, PulseHttpError> {
    api.alerts(
        account,
        query.acknowledged,
        page_request(query.cursor, query.limit),
    )
    .await
    .map(Json)
    .map_err(Into::into)
}

async fn rest_acknowledge(
    State(api): State<PulseApi>,
    Path((account, alert_id)): Path<(i64, i64)>,
) -> Result<StatusCode, PulseHttpError> {
    api.acknowledge_alert(account, alert_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Into::into)
}

async fn rest_reply(
    State(api): State<PulseApi>,
    Path((account, alert_id)): Path<(i64, i64)>,
    Json(body): Json<ReplyBody>,
) -> Result<Json<ReplyResult>, PulseHttpError> {
    api.reply_alert(account, alert_id, body.message)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn rest_alert_replies(
    State(api): State<PulseApi>,
    Path((account, alert_id)): Path<(i64, i64)>,
    Query(page): Query<PageRequest>,
) -> Result<Json<Page<AlertReply>>, PulseHttpError> {
    api.alert_replies(account, alert_id, page)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn rest_subscriptions(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(page): Query<PageRequest>,
) -> Result<Json<Page<StoredAlertSubscription>>, PulseHttpError> {
    api.subscriptions(account, page)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn rest_create_subscription(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Json(input): Json<AlertSubscriptionInput>,
) -> Result<(StatusCode, Json<StoredAlertSubscription>), PulseHttpError> {
    api.create_subscription(account, input)
        .await
        .map(|stored| (StatusCode::CREATED, Json(stored)))
        .map_err(Into::into)
}

async fn rest_delete_subscription(
    State(api): State<PulseApi>,
    Path((account, subscription_id)): Path<(i64, i64)>,
) -> Result<StatusCode, PulseHttpError> {
    api.delete_subscription(account, subscription_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Into::into)
}

async fn rest_pricing(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(page): Query<PageRequest>,
) -> Result<Json<Page<PublicPricingRule>>, PulseHttpError> {
    api.pricing(account, page)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn rest_upsert_pricing(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Json(input): Json<PricingRuleInput>,
) -> Result<StatusCode, PulseHttpError> {
    api.upsert_pricing(account, input)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Into::into)
}

async fn rest_delete_pricing(
    State(api): State<PulseApi>,
    Path((account, key)): Path<(i64, String)>,
) -> Result<StatusCode, PulseHttpError> {
    api.delete_pricing_override(account, key)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Into::into)
}

async fn rest_limits(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
) -> Result<Json<PulseLimits>, PulseHttpError> {
    api.limits(account).map(Json).map_err(Into::into)
}

async fn rest_machines(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(page): Query<PageRequest>,
) -> Result<Json<Page<super::Machine>>, PulseHttpError> {
    api.machines(account, page)
        .await
        .map(Json)
        .map_err(Into::into)
}

#[derive(Clone, Debug, Deserialize)]
pub struct IssueIngestTokenBody {
    pub machine: String,
}

async fn rest_ingest_tokens(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Query(page): Query<PageRequest>,
) -> Result<Json<Page<IngestTokenSummary>>, PulseHttpError> {
    api.ingest_tokens(account, page)
        .await
        .map(Json)
        .map_err(Into::into)
}

async fn rest_issue_ingest_token(
    State(api): State<PulseApi>,
    Path(account): Path<i64>,
    Json(body): Json<IssueIngestTokenBody>,
) -> Result<(StatusCode, Json<IssuedIngestTokenResponse>), PulseHttpError> {
    api.issue_ingest_token(account, body.machine)
        .await
        .map(|issued| (StatusCode::CREATED, Json(issued)))
        .map_err(Into::into)
}

async fn rest_revoke_ingest_token(
    State(api): State<PulseApi>,
    Path((account, token_id)): Path<(i64, i64)>,
) -> Result<StatusCode, PulseHttpError> {
    api.revoke_ingest_token(account, token_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Into::into)
}

fn paginate<T>(rows: Vec<T>, request: PageRequest) -> PulseResult<Page<T>> {
    paginate_validated(bounded_rows(rows, "API list")?, request.validate()?)
}

fn paginate_validated<T>(mut rows: Vec<T>, page: ValidatedPage) -> PulseResult<Page<T>> {
    if page.offset > rows.len() {
        return Err(PulseError::invalid_input(
            "page cursor is beyond the available result set",
        ));
    }
    let has_more = rows.len() > page.offset.saturating_add(page.limit);
    let end = page.offset.saturating_add(page.limit).min(rows.len());
    let items = rows.drain(page.offset..end).collect();
    let next_cursor = has_more
        .then_some(end)
        .filter(|offset| *offset <= MAX_CURSOR_OFFSET)
        .map(|offset| offset.to_string());
    Ok(Page { items, next_cursor })
}

fn parse_cursor(cursor: &str) -> PulseResult<usize> {
    if cursor.is_empty() || cursor.len() > 4 || !cursor.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PulseError::invalid_input("page cursor is invalid"));
    }
    let offset = cursor
        .parse::<usize>()
        .map_err(|_| PulseError::invalid_input("page cursor is invalid"))?;
    if offset > MAX_CURSOR_OFFSET {
        return Err(PulseError::invalid_input("page cursor exceeds its bound"));
    }
    Ok(offset)
}

fn bounded_rows<T>(rows: Vec<T>, name: &str) -> PulseResult<Vec<T>> {
    if rows.len() > MAX_LIST_ROWS {
        return Err(work_bound(name));
    }
    Ok(rows)
}

fn work_bound(name: &str) -> PulseError {
    PulseError::new(
        PulseErrorKind::Storage,
        format!("Pulse {name} exceeded its query bound"),
    )
}

fn not_found(message: &'static str) -> PulseError {
    PulseError::new(PulseErrorKind::NotFound, message)
}

fn positive_id(value: i64, name: &str) -> PulseResult<()> {
    if value <= 0 {
        return Err(PulseError::invalid_input(format!(
            "{name} must be positive"
        )));
    }
    Ok(())
}

fn validate_reply(message: &str) -> PulseResult<()> {
    if message.is_empty()
        || message.len() > MAX_ALERT_REPLY_BYTES
        || message.trim() != message
        || message.chars().any(char::is_control)
    {
        return Err(PulseError::invalid_input(
            "alert reply must be 1..=2048 bytes without padding or control characters",
        ));
    }
    Ok(())
}

fn required<T>(value: Option<T>, message: &'static str) -> PulseResult<T> {
    value.ok_or_else(|| PulseError::invalid_input(message))
}

fn today() -> String {
    Instant::now()
        .to_iso8601()
        .get(..10)
        .unwrap_or("1970-01-01")
        .to_owned()
}

fn ok_json() -> serde_json::Value {
    serde_json::json!({"ok": true})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_and_page_sizes_are_strictly_bounded() {
        assert!(PageRequest::default().validate().is_ok());
        assert!(
            PageRequest {
                cursor: Some("9901".to_owned()),
                limit: Some(1),
            }
            .validate()
            .is_err()
        );
        assert!(
            PageRequest {
                cursor: Some("-1".to_owned()),
                limit: Some(1),
            }
            .validate()
            .is_err()
        );
        assert!(
            PageRequest {
                cursor: None,
                limit: Some(101),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn pagination_uses_an_opaque_bounded_cursor() {
        let first = paginate(
            (0..5).collect::<Vec<_>>(),
            PageRequest {
                cursor: None,
                limit: Some(2),
            },
        )
        .unwrap();
        assert_eq!(first.items, vec![0, 1]);
        assert_eq!(first.next_cursor.as_deref(), Some("2"));
        let second = paginate(
            (0..5).collect::<Vec<_>>(),
            PageRequest {
                cursor: first.next_cursor,
                limit: Some(2),
            },
        )
        .unwrap();
        assert_eq!(second.items, vec![2, 3]);
    }

    #[test]
    fn replies_are_validated_without_retaining_text() {
        assert!(validate_reply("handled").is_ok());
        assert!(validate_reply("").is_err());
        assert!(validate_reply(" padded ").is_err());
        assert!(validate_reply("line\nbreak").is_err());
        assert!(validate_reply(&"x".repeat(MAX_ALERT_REPLY_BYTES + 1)).is_err());
    }

    #[test]
    fn auth_failure_keeps_frozen_wire_name() {
        let input: AlertSubscriptionInput = serde_json::from_value(serde_json::json!({
            "profile": "claude-max",
            "alert_type": "auth_failure"
        }))
        .unwrap();
        assert_eq!(input.alert_type, AlertType::AuthenticationFailure);
        assert!(input.threshold.is_none());
    }

    #[test]
    fn profile_budget_patch_distinguishes_missing_clear_and_set() {
        let missing: ProfileSettingsInput =
            serde_json::from_value(serde_json::json!({"poll_interval_minutes": 30})).unwrap();
        assert_eq!(missing.monthly_budget_usd, MonthlyBudgetPatch::Missing);
        let clear: ProfileSettingsInput =
            serde_json::from_value(serde_json::json!({"monthly_budget_usd": null})).unwrap();
        assert_eq!(clear.monthly_budget_usd, MonthlyBudgetPatch::Clear);
        let set: ProfileSettingsInput =
            serde_json::from_value(serde_json::json!({"monthly_budget_usd": 42.5})).unwrap();
        assert_eq!(set.monthly_budget_usd, MonthlyBudgetPatch::Set(42.5));
    }

    #[test]
    fn pulse_failures_map_without_confusing_provider_auth_with_api_auth() {
        let status = |kind| {
            PulseHttpError(PulseError::new(kind, "safe failure"))
                .into_response()
                .status()
        };
        assert_eq!(
            status(PulseErrorKind::InvalidInput),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(status(PulseErrorKind::NotFound), StatusCode::NOT_FOUND);
        assert_eq!(status(PulseErrorKind::Conflict), StatusCode::CONFLICT);
        assert_eq!(
            status(PulseErrorKind::RateLimited),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            status(PulseErrorKind::Authentication),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            status(PulseErrorKind::Offline),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
