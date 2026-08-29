use std::{collections::BTreeMap, fmt, path::PathBuf, str::FromStr};

use jiff::civil::Date;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    error::{PulseError, PulseResult},
    time::Instant,
};

const MAX_PROFILE_NAME_BYTES: usize = 128;
const MAX_MACHINE_NAME_BYTES: usize = 255;
const MAX_SESSION_ID_BYTES: usize = 512;
const MAX_MODEL_NAME_BYTES: usize = 256;
pub const MIN_PROFILE_POLL_MINUTES: u32 = 5;
pub const MAX_PROFILE_POLL_MINUTES: u32 = 7 * 24 * 60;

fn validate_text(kind: &str, value: &str, max_bytes: usize) -> PulseResult<()> {
    if value.is_empty() {
        return Err(PulseError::invalid_input(format!("{kind} cannot be empty")));
    }
    if value.len() > max_bytes {
        return Err(PulseError::invalid_input(format!(
            "{kind} exceeds {max_bytes} bytes"
        )));
    }
    if value.trim() != value {
        return Err(PulseError::invalid_input(format!(
            "{kind} cannot start or end with whitespace"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(PulseError::invalid_input(format!(
            "{kind} cannot contain control characters"
        )));
    }
    Ok(())
}

macro_rules! validated_string {
    ($name:ident, $label:literal, $max:expr) => {
        #[derive(
            Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
        )]
        #[serde(try_from = "String", into = "String")]
        #[schemars(with = "String")]
        pub struct $name(String);

        impl $name {
            /// Creates a validated identifier.
            ///
            /// # Errors
            ///
            /// Returns an invalid-input error for empty, oversized, padded, or
            /// control-character-containing values.
            pub fn new(value: impl Into<String>) -> PulseResult<Self> {
                let value = value.into();
                validate_text($label, &value, $max)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = PulseError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
    };
}

validated_string!(ProfileName, "profile name", MAX_PROFILE_NAME_BYTES);
validated_string!(MachineName, "machine name", MAX_MACHINE_NAME_BYTES);
validated_string!(SessionId, "session id", MAX_SESSION_ID_BYTES);

/// Positive database identity of one Pulse account.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "i64", into = "i64")]
#[schemars(with = "i64")]
pub struct AccountId(i64);

impl AccountId {
    /// Creates a positive account id.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when `value` is not positive.
    pub fn new(value: i64) -> PulseResult<Self> {
        if value <= 0 {
            return Err(PulseError::invalid_input("account id must be positive"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl TryFrom<i64> for AccountId {
    type Error = PulseError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<AccountId> for i64 {
    fn from(value: AccountId) -> Self {
        value.0
    }
}

/// A finite percentage in the inclusive range 0–100.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64", into = "f64")]
#[schemars(with = "f64")]
pub struct Percent(f64);

impl Percent {
    /// Creates a bounded percentage.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for NaN, infinity, or a value outside
    /// the inclusive range 0–100.
    pub fn new(value: f64) -> PulseResult<Self> {
        if !value.is_finite() || !(0.0..=100.0).contains(&value) {
            return Err(PulseError::invalid_input(
                "percentage must be finite and between 0 and 100",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for Percent {
    type Error = PulseError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Percent> for f64 {
    fn from(value: Percent) -> Self {
        value.0
    }
}

/// A finite fraction in the inclusive range 0–1.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64", into = "f64")]
#[schemars(with = "f64")]
pub struct Fraction(f64);

impl Fraction {
    /// Creates a bounded fraction.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for NaN, infinity, or a value outside
    /// the inclusive range 0–1.
    pub fn new(value: f64) -> PulseResult<Self> {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(PulseError::invalid_input(
                "fraction must be finite and between 0 and 1",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for Fraction {
    type Error = PulseError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Fraction> for f64 {
    fn from(value: Fraction) -> Self {
        value.0
    }
}

/// Supported quota and token providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Vendor {
    AnthropicOauth,
    OpenaiCodex,
    DeepseekBalance,
    XaiGrok,
    Gemini,
    Antigravity,
}

impl Vendor {
    /// Whether same-period quota decreases are stale for this vendor.
    #[must_use]
    pub const fn rejects_same_period_decrease(self, window: QuotaWindowKind) -> bool {
        !matches!(
            (self, window),
            (Self::AnthropicOauth, QuotaWindowKind::RollingSevenDay)
        )
    }

    /// Whether this provider emits subscription rate-limit snapshots.
    #[must_use]
    pub const fn emits_usage_snapshots(self) -> bool {
        !matches!(self, Self::Gemini | Self::Antigravity)
    }

    /// Whether a quota window has the right semantics for this vendor.
    #[must_use]
    pub const fn allows_window(self, window: QuotaWindowKind) -> bool {
        matches!(
            (self, window),
            (
                Self::AnthropicOauth,
                QuotaWindowKind::FiveHour | QuotaWindowKind::RollingSevenDay
            ) | (
                Self::OpenaiCodex,
                QuotaWindowKind::FiveHour | QuotaWindowKind::FixedWeekly
            ) | (Self::XaiGrok, QuotaWindowKind::FixedWeekly)
                | (Self::DeepseekBalance, QuotaWindowKind::MonthlyBudget)
        )
    }
}

/// Credential refresh behavior. Credential values never appear in this model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RefreshPolicy {
    Never,
    #[default]
    InMemory,
    Persist,
}

/// Trust boundary for an account-scoped provider profile.
///
/// Local profiles may reference credentials and local paths. Reported profiles
/// are admitted from authenticated ingest and contain safe discovery metadata
/// only; collectors must never execute them locally.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProfileOrigin {
    #[default]
    Local,
    Reported,
}

/// Account identity visible to the single-operator Pulse service.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Account {
    pub id: AccountId,
    pub identity: String,
    pub display_name: Option<String>,
}

impl Account {
    /// Validates externally supplied account display fields.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for malformed identity text.
    pub fn validate(&self) -> PulseResult<()> {
        validate_text("account identity", &self.identity, 320)?;
        if let Some(display_name) = &self.display_name {
            validate_text("account display name", display_name, 320)?;
        }
        Ok(())
    }
}

/// One machine contributing data to an account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Machine {
    pub account_id: AccountId,
    pub name: MachineName,
    pub first_seen: Instant,
    pub last_seen: Instant,
}

/// Account-scoped provider profile.
///
/// API keys are referenced through an environment variable or absolute file;
/// there is intentionally no field capable of holding an inline secret.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Profile {
    pub account_id: AccountId,
    pub name: ProfileName,
    pub vendor: Vendor,
    pub config_dir: Option<PathBuf>,
    #[serde(default = "default_profile_poll_minutes")]
    pub poll_interval_minutes: u32,
    pub monthly_budget_usd: Option<f64>,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<PathBuf>,
    #[serde(default)]
    pub refresh: RefreshPolicy,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub origin: ProfileOrigin,
}

const fn default_profile_poll_minutes() -> u32 {
    15
}

impl Profile {
    /// Validates polling bounds and secret references.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for ambiguous or unsafe credential
    /// references and provider-specific missing settings.
    pub fn validate(&self) -> PulseResult<()> {
        if !(MIN_PROFILE_POLL_MINUTES..=MAX_PROFILE_POLL_MINUTES)
            .contains(&self.poll_interval_minutes)
        {
            return Err(PulseError::configuration(format!(
                "profile poll interval must be between {MIN_PROFILE_POLL_MINUTES} and \
                 {MAX_PROFILE_POLL_MINUTES} minutes"
            )));
        }
        if self.api_key_env.is_some() && self.api_key_file.is_some() {
            return Err(PulseError::configuration(
                "configure only one of api_key_env or api_key_file",
            ));
        }
        if let Some(name) = &self.api_key_env {
            validate_environment_name(name)?;
        }
        if let Some(path) = &self.api_key_file
            && !path.is_absolute()
        {
            return Err(PulseError::configuration(
                "api_key_file must be an absolute path",
            ));
        }
        if let Some(budget) = self.monthly_budget_usd
            && (!budget.is_finite() || budget <= 0.0)
        {
            return Err(PulseError::configuration(
                "monthly_budget_usd must be finite and greater than zero",
            ));
        }
        if self.origin == ProfileOrigin::Reported {
            if self.config_dir.is_some()
                || self.api_key_env.is_some()
                || self.api_key_file.is_some()
                || self.refresh == RefreshPolicy::Persist
            {
                return Err(PulseError::configuration(
                    "reported profiles cannot contain local paths, credential references, or persistent refresh",
                ));
            }
        } else if self.vendor == Vendor::DeepseekBalance {
            if self.monthly_budget_usd.is_none() {
                return Err(PulseError::configuration(
                    "DeepSeek profiles require monthly_budget_usd",
                ));
            }
            if self.api_key_env.is_none() && self.api_key_file.is_none() {
                return Err(PulseError::configuration(
                    "DeepSeek profiles require api_key_env or api_key_file",
                ));
            }
        }
        #[cfg(target_os = "macos")]
        if self.refresh == RefreshPolicy::Persist {
            return Err(PulseError::configuration(
                "persistent credential refresh is disabled on macOS",
            ));
        }
        Ok(())
    }
}

fn validate_environment_name(value: &str) -> PulseResult<()> {
    let mut chars = value.chars();
    let valid_first = chars
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
    if !valid_first || !chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(PulseError::configuration(
            "credential environment variable name is invalid",
        ));
    }
    Ok(())
}

/// Semantic kind of one provider quota window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotaWindowKind {
    FiveHour,
    RollingSevenDay,
    FixedWeekly,
    MonthlyBudget,
}

/// A typed projection of one quota window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QuotaWindow {
    pub kind: QuotaWindowKind,
    pub used_percent: Percent,
    pub resets_at: Instant,
}

/// Secret-free outcome of a collection attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CollectionOutcome {
    Success,
    Disabled { code: String },
    AuthenticationFailed { code: String },
    RateLimited { retry_at: Option<Instant> },
    Unavailable { code: String },
    InvalidResponse { code: String },
}

impl CollectionOutcome {
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    fn validate(&self) -> PulseResult<()> {
        let code = match self {
            Self::Success | Self::RateLimited { .. } => return Ok(()),
            Self::Disabled { code }
            | Self::AuthenticationFailed { code }
            | Self::Unavailable { code }
            | Self::InvalidResponse { code } => code,
        };
        let valid = !code.is_empty()
            && code.len() <= 64
            && code.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'-' | b'.')
            });
        if !valid {
            return Err(PulseError::invalid_input(
                "collection outcome code must be a stable lowercase identifier",
            ));
        }
        Ok(())
    }
}

/// An append-only provider usage observation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UsageSnapshot {
    pub account_id: AccountId,
    pub profile: ProfileName,
    pub machine: MachineName,
    pub vendor: Vendor,
    pub windows: Vec<QuotaWindow>,
    pub outcome: CollectionOutcome,
    pub polled_at: Instant,
    pub reporter_version: Option<String>,
}

impl UsageSnapshot {
    /// Validates provider semantics and duplicate window kinds.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when successful data is empty, when a
    /// non-quota vendor emits a snapshot, or when window kinds are duplicated.
    pub fn validate(&self) -> PulseResult<()> {
        self.outcome.validate()?;
        if !self.vendor.emits_usage_snapshots() {
            return Err(PulseError::invalid_input(
                "this vendor does not emit subscription usage snapshots",
            ));
        }
        if self.outcome.is_success() && self.windows.is_empty() {
            return Err(PulseError::invalid_input(
                "a successful usage snapshot must contain a quota window",
            ));
        }
        if self
            .windows
            .iter()
            .any(|window| !self.vendor.allows_window(window.kind))
        {
            return Err(PulseError::invalid_input(
                "usage snapshot contains a window that is invalid for its vendor",
            ));
        }
        let mut kinds = self
            .windows
            .iter()
            .map(|window| window.kind)
            .collect::<Vec<_>>();
        kinds.sort_unstable_by_key(|kind| *kind as u8);
        if kinds.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(PulseError::invalid_input(
                "usage snapshot contains a duplicate quota window",
            ));
        }
        if let Some(version) = &self.reporter_version {
            validate_text("reporter version", version, 128)?;
        }
        Ok(())
    }
}

/// Provenance attached to an account-global quota card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UsageContributor {
    pub machine: MachineName,
    pub reporter_version: Option<String>,
    pub polled_at: Instant,
    pub chosen: bool,
}

/// Origin of a token observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    Local,
    Ingest,
}

/// Model settings that can affect price or token behavior.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentSettings {
    pub service_tier: Option<String>,
    pub effort: Option<String>,
    #[serde(default)]
    pub additional: BTreeMap<String, String>,
}

impl AgentSettings {
    /// Stable SHA-256 of the canonical, key-ordered JSON representation.
    ///
    /// # Errors
    ///
    /// Returns an internal error if serialization unexpectedly fails.
    pub fn sha256(&self) -> PulseResult<String> {
        let bytes = serde_json::to_vec(self).map_err(|error| {
            PulseError::new(
                super::error::PulseErrorKind::Internal,
                format!("failed to encode agent settings: {error}"),
            )
        })?;
        let digest = Sha256::digest(bytes);
        let mut encoded = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use fmt::Write as _;
            write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(encoded)
    }
}

/// Fine token usage at the profile/machine/session/model/settings/day grain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenGrain {
    pub account_id: AccountId,
    pub profile: ProfileName,
    pub machine: MachineName,
    pub session_id: SessionId,
    pub model: String,
    pub settings: AgentSettings,
    pub settings_hash: String,
    pub day: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
    pub source: TokenSource,
}

impl TokenGrain {
    /// Validates dimensions, date, and settings integrity.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for malformed dimensions or a settings
    /// digest that does not match the canonical settings JSON.
    pub fn validate(&self) -> PulseResult<()> {
        validate_text("model", &self.model, MAX_MODEL_NAME_BYTES)?;
        Date::from_str(&self.day)
            .map_err(|error| PulseError::invalid_input(format!("invalid token day: {error}")))?;
        if self.settings_hash != self.settings.sha256()? {
            return Err(PulseError::invalid_input(
                "settings_hash does not match settings",
            ));
        }
        Ok(())
    }

    /// Total counted tokens, saturating only when corrupt inputs exceed `u64`.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.tokens_in
            .saturating_add(self.tokens_out)
            .saturating_add(self.cache_write_5m)
            .saturating_add(self.cache_write_1h)
            .saturating_add(self.cache_read)
    }
}

/// Live context usage for one local agent session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContextSession {
    pub account_id: AccountId,
    pub profile: ProfileName,
    pub machine: MachineName,
    pub session_id: SessionId,
    pub model: Option<String>,
    pub settings: AgentSettings,
    pub context_tokens: Option<u64>,
    pub context_percent: Option<Percent>,
    pub effective_limit: Option<u64>,
    pub last_active_at: Instant,
    pub last_reset_at: Option<Instant>,
    pub collected_at: Instant,
}

impl ContextSession {
    /// Validates model text and consistency of context measurements.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for malformed or inconsistent fields.
    pub fn validate(&self) -> PulseResult<()> {
        if let Some(model) = &self.model {
            validate_text("model", model, MAX_MODEL_NAME_BYTES)?;
        }
        if let (Some(tokens), Some(limit)) = (self.context_tokens, self.effective_limit)
            && tokens > limit
        {
            return Err(PulseError::invalid_input(
                "context token count cannot exceed its effective limit",
            ));
        }
        if self.effective_limit == Some(0) {
            return Err(PulseError::invalid_input(
                "context effective limit must be greater than zero",
            ));
        }
        if self.context_percent.is_some()
            && (self.context_tokens.is_none() || self.effective_limit.is_none())
        {
            return Err(PulseError::invalid_input(
                "context percentage requires tokens and effective limit",
            ));
        }
        Ok(())
    }
}

/// Account-level Gemini quota bucket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GeminiQuota {
    pub account_id: AccountId,
    pub model_id: String,
    pub remaining_fraction: Fraction,
    pub remaining_amount: Option<String>,
    pub resets_at: Option<Instant>,
    pub collected_at: Instant,
}

impl GeminiQuota {
    /// Validates the model bucket identifier.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for a malformed identifier.
    pub fn validate(&self) -> PulseResult<()> {
        validate_text("Gemini model id", &self.model_id, MAX_MODEL_NAME_BYTES)
    }
}

/// Alert signal types supported by Pulse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertType {
    FiveHourThreshold,
    SevenDayThreshold,
    #[serde(rename = "auth_failure")]
    AuthenticationFailure,
    ContextThreshold,
}

/// Opt-in alert delivery. Pull-based API visibility requires no delivery value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AlertDelivery {
    Channel,
    Pane { pane_id: String },
}

/// Natural-key portion of an account-scoped alert subscription.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AlertSubscription {
    pub account_id: AccountId,
    pub profile: ProfileName,
    pub alert_type: AlertType,
    pub threshold: Option<Percent>,
    #[serde(default = "default_alert_cooldown_minutes")]
    pub cooldown_minutes: u32,
    pub delivery: Option<AlertDelivery>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

const fn default_alert_cooldown_minutes() -> u32 {
    30
}

const fn default_true() -> bool {
    true
}

impl AlertSubscription {
    /// Validates threshold and side-effect delivery rules.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for missing thresholds, zero cooldowns,
    /// or pane delivery on authentication failures.
    pub fn validate(&self) -> PulseResult<()> {
        let needs_threshold = matches!(
            self.alert_type,
            AlertType::FiveHourThreshold
                | AlertType::SevenDayThreshold
                | AlertType::ContextThreshold
        );
        if needs_threshold != self.threshold.is_some() {
            return Err(PulseError::invalid_input(
                "threshold alerts require a threshold and non-threshold alerts forbid one",
            ));
        }
        if self.cooldown_minutes == 0 {
            return Err(PulseError::invalid_input(
                "alert cooldown must be at least one minute",
            ));
        }
        if self.alert_type == AlertType::AuthenticationFailure
            && matches!(self.delivery, Some(AlertDelivery::Pane { .. }))
        {
            return Err(PulseError::invalid_input(
                "authentication failures cannot be delivered into an agent pane",
            ));
        }
        if let Some(AlertDelivery::Pane { pane_id }) = &self.delivery {
            validate_text("pane id", pane_id, 128)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_id() -> AccountId {
        AccountId::new(1).expect("valid account")
    }

    fn profile_name() -> ProfileName {
        ProfileName::new("claude-max").expect("valid profile")
    }

    #[test]
    fn identifiers_and_numeric_bounds_are_validated_during_deserialization() {
        assert!(serde_json::from_str::<AccountId>("0").is_err());
        assert!(serde_json::from_str::<ProfileName>("\" padded \"").is_err());
        assert!(serde_json::from_str::<Percent>("101").is_err());
        assert!(serde_json::from_str::<Fraction>("-0.01").is_err());
    }

    #[test]
    fn vendor_staleness_is_window_aware() {
        assert!(
            !Vendor::AnthropicOauth.rejects_same_period_decrease(QuotaWindowKind::RollingSevenDay)
        );
        assert!(Vendor::AnthropicOauth.rejects_same_period_decrease(QuotaWindowKind::FiveHour));
        assert!(Vendor::OpenaiCodex.rejects_same_period_decrease(QuotaWindowKind::FiveHour));
        assert!(Vendor::OpenaiCodex.rejects_same_period_decrease(QuotaWindowKind::FixedWeekly));
        assert!(
            Vendor::DeepseekBalance.rejects_same_period_decrease(QuotaWindowKind::MonthlyBudget)
        );
        assert!(Vendor::AnthropicOauth.allows_window(QuotaWindowKind::RollingSevenDay));
        assert!(!Vendor::AnthropicOauth.allows_window(QuotaWindowKind::FixedWeekly));
        assert!(Vendor::OpenaiCodex.allows_window(QuotaWindowKind::FiveHour));
        assert!(Vendor::OpenaiCodex.allows_window(QuotaWindowKind::FixedWeekly));
    }

    #[test]
    fn profile_has_references_not_inline_secrets() {
        let profile = Profile {
            account_id: account_id(),
            name: ProfileName::new("deepseek").expect("valid profile"),
            vendor: Vendor::DeepseekBalance,
            config_dir: None,
            poll_interval_minutes: 15,
            monthly_budget_usd: Some(100.0),
            api_key_env: Some("DEEPSEEK_API_KEY".to_owned()),
            api_key_file: None,
            refresh: RefreshPolicy::Never,
            hidden: false,
            origin: ProfileOrigin::Local,
        };
        profile.validate().expect("valid profile");
        let json = serde_json::to_string(&profile).expect("serialize");
        assert!(json.contains("DEEPSEEK_API_KEY"));
        assert!(!json.contains("api_key\""));

        let mut ambiguous = profile;
        ambiguous.api_key_file = Some(PathBuf::from("/run/secrets/deepseek"));
        assert!(ambiguous.validate().is_err());
    }

    #[test]
    fn reported_profiles_forbid_local_execution_material() {
        let reported = Profile {
            account_id: account_id(),
            name: profile_name(),
            vendor: Vendor::AnthropicOauth,
            config_dir: None,
            poll_interval_minutes: 15,
            monthly_budget_usd: None,
            api_key_env: None,
            api_key_file: None,
            refresh: RefreshPolicy::Never,
            hidden: false,
            origin: ProfileOrigin::Reported,
        };
        reported.validate().expect("safe reported metadata");
        assert_eq!(
            serde_json::from_str::<ProfileOrigin>("\"local\"").expect("origin"),
            ProfileOrigin::Local
        );
        assert!(
            Profile {
                config_dir: Some(PathBuf::from("/tmp/reported")),
                ..reported
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn usage_snapshot_requires_typed_nonduplicate_windows() {
        let instant = Instant::from_epoch_millis(1_786_214_400_000).expect("valid instant");
        let window = QuotaWindow {
            kind: QuotaWindowKind::FiveHour,
            used_percent: Percent::new(42.0).expect("valid percent"),
            resets_at: instant,
        };
        let mut snapshot = UsageSnapshot {
            account_id: account_id(),
            profile: profile_name(),
            machine: MachineName::new("midnight").expect("valid machine"),
            vendor: Vendor::AnthropicOauth,
            windows: vec![window.clone()],
            outcome: CollectionOutcome::Success,
            polled_at: instant,
            reporter_version: Some("0.1.0+abc123".to_owned()),
        };
        snapshot.validate().expect("valid snapshot");
        snapshot.windows.push(window);
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn token_grain_checks_canonical_settings_hash_and_day() {
        let settings = AgentSettings {
            service_tier: Some("priority".to_owned()),
            effort: Some("xhigh".to_owned()),
            additional: BTreeMap::from([("reasoning".to_owned(), "full".to_owned())]),
        };
        let mut grain = TokenGrain {
            account_id: account_id(),
            profile: profile_name(),
            machine: MachineName::new("max").expect("valid machine"),
            session_id: SessionId::new("session-1").expect("valid session"),
            model: "claude-opus-5".to_owned(),
            settings_hash: settings.sha256().expect("hash"),
            settings,
            day: "2026-08-08".to_owned(),
            tokens_in: 1,
            tokens_out: 2,
            cache_write_5m: 3,
            cache_write_1h: 4,
            cache_read: 5,
            source: TokenSource::Local,
        };
        grain.validate().expect("valid token grain");
        assert_eq!(grain.total_tokens(), 15);
        grain.settings_hash = "tampered".to_owned();
        assert!(grain.validate().is_err());
    }

    #[test]
    fn pane_delivery_is_forbidden_for_auth_failures() {
        let alert = AlertSubscription {
            account_id: account_id(),
            profile: profile_name(),
            alert_type: AlertType::AuthenticationFailure,
            threshold: None,
            cooldown_minutes: 30,
            delivery: Some(AlertDelivery::Pane {
                pane_id: "%42".to_owned(),
            }),
            enabled: true,
        };
        assert!(alert.validate().is_err());
    }

    #[test]
    fn authentication_alert_keeps_the_pulse_wire_name() {
        assert_eq!(
            serde_json::to_string(&AlertType::AuthenticationFailure).expect("serialize"),
            "\"auth_failure\""
        );
        assert_eq!(
            serde_json::from_str::<AlertType>("\"auth_failure\"").expect("deserialize"),
            AlertType::AuthenticationFailure
        );
        assert!(serde_json::from_str::<AlertType>("\"authentication_failure\"").is_err());
    }
}
