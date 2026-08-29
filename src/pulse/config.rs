use std::path::PathBuf;

use hyper::Uri;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    error::{PulseError, PulseResult},
    model::{Account, AccountId, Profile, ProfileName, ProfileOrigin, RefreshPolicy, Vendor},
};

const MIN_USAGE_SECONDS: u64 = 5 * 60;
const MIN_CONTEXT_SECONDS: u64 = 30;
const MIN_TOKEN_SECONDS: u64 = 5 * 60;
const MIN_GEMINI_SECONDS: u64 = 30;
const MIN_RETENTION_SECONDS: u64 = 60;
const DEFAULT_FEDERATION_INTERVAL_SECONDS: u64 = 300;
const MIN_FEDERATION_INTERVAL_SECONDS: u64 = 30;
const MAX_FEDERATION_INTERVAL_SECONDS: u64 = 24 * 60 * 60;
pub const MAX_BOOTSTRAP_ACCOUNTS: usize = 32;
pub const MAX_BOOTSTRAP_PROFILES_PER_ACCOUNT: usize = 256;

/// Native Pulse runtime capabilities and safe external references.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct PulseConfig {
    /// Collect local provider and transcript observations.
    pub collect: bool,
    /// Serve Pulse through atmux's already-authenticated REST/MCP surfaces.
    pub serve: bool,
    /// Accept separately authenticated push ingest. Off by default.
    pub receive: bool,
    /// Rescan interval for authenticated configured atmux peers. Omitted uses
    /// five minutes; `None` in programmatic defaults has the same effect.
    #[serde(default = "default_federation_interval")]
    pub federation_interval_seconds: Option<u64>,
    /// Optional HTTPS receiver for machines that federation cannot reach.
    pub report_to: Option<String>,
    /// Environment variable containing the report bearer token.
    pub report_token_env: Option<String>,
    /// Absolute file containing the report bearer token.
    pub report_token_file: Option<PathBuf>,
    /// Environment variable containing the distinct outer node/proxy token.
    pub report_node_token_env: Option<String>,
    /// Absolute file containing the distinct outer node/proxy token.
    pub report_node_token_file: Option<PathBuf>,
    pub database: PulseDatabaseConfig,
    pub schedule: PulseScheduleConfig,
    pub credentials: PulseCredentialConfig,
    pub retention: PulseRetentionConfig,
    /// Explicit identities visible to REST/MCP and eligible for collection.
    pub accounts: Vec<PulseAccountConfig>,
}

impl PulseConfig {
    /// Validates capability combinations, secret references, and nested bounds.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an unsafe endpoint, inline/ambiguous
    /// secret reference, impossible capability combination, or invalid bound.
    pub fn validate(&self) -> PulseResult<()> {
        if self.receive && !self.serve {
            return Err(PulseError::configuration(
                "pulse.receive requires pulse.serve",
            ));
        }
        if self.federation_interval_seconds.is_some_and(|seconds| {
            !(MIN_FEDERATION_INTERVAL_SECONDS..=MAX_FEDERATION_INTERVAL_SECONDS).contains(&seconds)
        }) {
            return Err(PulseError::configuration(format!(
                "pulse.federation_interval_seconds must be between \
                 {MIN_FEDERATION_INTERVAL_SECONDS} and {MAX_FEDERATION_INTERVAL_SECONDS}"
            )));
        }
        if self.report_token_env.is_some() && self.report_token_file.is_some() {
            return Err(PulseError::configuration(
                "configure only one of pulse.report_token_env or pulse.report_token_file",
            ));
        }
        if self.report_node_token_env.is_some() && self.report_node_token_file.is_some() {
            return Err(PulseError::configuration(
                "configure only one of pulse.report_node_token_env or pulse.report_node_token_file",
            ));
        }
        if self.report_token_env.is_some() && self.report_token_env == self.report_node_token_env
            || self.report_token_file.is_some()
                && self.report_token_file == self.report_node_token_file
        {
            return Err(PulseError::configuration(
                "Pulse ingest and node tokens must use distinct external references",
            ));
        }
        if let Some(name) = &self.report_token_env {
            validate_environment_name(name)?;
        }
        if let Some(name) = &self.report_node_token_env {
            validate_environment_name(name)?;
        }
        if let Some(path) = &self.report_token_file
            && !path.is_absolute()
        {
            return Err(PulseError::configuration(
                "pulse.report_token_file must be an absolute path",
            ));
        }
        if let Some(path) = &self.report_node_token_file
            && !path.is_absolute()
        {
            return Err(PulseError::configuration(
                "pulse.report_node_token_file must be an absolute path",
            ));
        }
        match &self.report_to {
            Some(target) => {
                let loopback = validate_report_target(target)?;
                if self.report_token_env.is_none() && self.report_token_file.is_none() {
                    return Err(PulseError::configuration(
                        "pulse.report_to requires an external report token reference",
                    ));
                }
                if !loopback
                    && self.report_node_token_env.is_none()
                    && self.report_node_token_file.is_none()
                {
                    return Err(PulseError::configuration(
                        "non-loopback pulse.report_to requires a separate external node token reference",
                    ));
                }
            }
            None if self.report_token_env.is_some()
                || self.report_token_file.is_some()
                || self.report_node_token_env.is_some()
                || self.report_node_token_file.is_some() =>
            {
                return Err(PulseError::configuration(
                    "a report token reference requires pulse.report_to",
                ));
            }
            None => {}
        }
        self.database.validate()?;
        self.schedule.validate()?;
        self.credentials.validate()?;
        self.retention.validate()?;
        validate_accounts(&self.accounts)?;
        if self.collect
            && self
                .accounts
                .iter()
                .flat_map(|account| &account.profiles)
                .any(|profile| profile.vendor == Vendor::Gemini)
            && (self.credentials.gemini_oauth_client_id_env.is_none()
                || self.credentials.gemini_oauth_client_secret_env.is_none())
        {
            return Err(PulseError::configuration(
                "local Gemini collection requires both external OAuth application environment references",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn effective_federation_interval_seconds(&self) -> u64 {
        match self.federation_interval_seconds {
            Some(seconds) => seconds,
            None => DEFAULT_FEDERATION_INTERVAL_SECONDS,
        }
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "serde default function must return the Option-valued field type"
)]
const fn default_federation_interval() -> Option<u64> {
    Some(DEFAULT_FEDERATION_INTERVAL_SECONDS)
}

/// One explicitly configured Pulse account.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PulseAccountConfig {
    pub id: i64,
    pub identity: String,
    pub display_name: Option<String>,
    #[serde(default)]
    pub profiles: Vec<PulseProfileConfig>,
}

impl PulseAccountConfig {
    /// Converts configuration into the validated domain account.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an invalid id or identity.
    pub fn account(&self) -> PulseResult<Account> {
        let account = Account {
            id: AccountId::new(self.id)?,
            identity: self.identity.clone(),
            display_name: self.display_name.clone(),
        };
        account.validate()?;
        Ok(account)
    }

    /// Converts every configured profile without resolving its secret refs.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for malformed profile settings or paths.
    pub fn domain_profiles(&self) -> PulseResult<Vec<Profile>> {
        let account_id = AccountId::new(self.id)?;
        self.profiles
            .iter()
            .map(|profile| profile.domain_profile(account_id))
            .collect()
    }
}

/// Secret-free profile settings. Credentials can only be referenced outside
/// the configuration document through an environment variable or file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PulseProfileConfig {
    pub name: String,
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
}

const fn default_profile_poll_minutes() -> u32 {
    15
}

impl PulseProfileConfig {
    fn domain_profile(&self, account_id: AccountId) -> PulseResult<Profile> {
        if self
            .config_dir
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(PulseError::configuration(
                "Pulse profile config_dir must be absolute",
            ));
        }
        let profile = Profile {
            account_id,
            name: ProfileName::new(self.name.clone())?,
            vendor: self.vendor,
            config_dir: self.config_dir.clone(),
            poll_interval_minutes: self.poll_interval_minutes,
            monthly_budget_usd: self.monthly_budget_usd,
            api_key_env: self.api_key_env.clone(),
            api_key_file: self.api_key_file.clone(),
            refresh: self.refresh,
            hidden: self.hidden,
            origin: ProfileOrigin::Local,
        };
        profile.validate()?;
        Ok(profile)
    }
}

fn validate_accounts(accounts: &[PulseAccountConfig]) -> PulseResult<()> {
    if accounts.len() > MAX_BOOTSTRAP_ACCOUNTS {
        return Err(PulseError::configuration(format!(
            "pulse.accounts cannot exceed {MAX_BOOTSTRAP_ACCOUNTS} entries"
        )));
    }
    let mut account_ids = std::collections::BTreeSet::new();
    let mut identities = std::collections::BTreeSet::new();
    for configured in accounts {
        let account = configured.account()?;
        if !account_ids.insert(account.id) {
            return Err(PulseError::configuration(
                "pulse.accounts contains a duplicate id",
            ));
        }
        if !identities.insert(account.identity) {
            return Err(PulseError::configuration(
                "pulse.accounts contains a duplicate identity",
            ));
        }
        if configured.profiles.len() > MAX_BOOTSTRAP_PROFILES_PER_ACCOUNT {
            return Err(PulseError::configuration(format!(
                "a Pulse account cannot exceed {MAX_BOOTSTRAP_PROFILES_PER_ACCOUNT} profiles"
            )));
        }
        let mut names = std::collections::BTreeSet::new();
        for profile in configured.domain_profiles()? {
            if !names.insert(profile.name) {
                return Err(PulseError::configuration(
                    "a Pulse account contains a duplicate profile name",
                ));
            }
        }
    }
    Ok(())
}

/// Storage selection. Connection secrets are always indirect.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct PulseDatabaseConfig {
    /// Explicit `SQLite` path; unset resolves below atmux's platform data dir.
    pub sqlite_path: Option<PathBuf>,
    /// Environment variable containing a `PostgreSQL` connection URL.
    pub postgres_url_env: Option<String>,
}

impl PulseDatabaseConfig {
    /// Validates the selected backend and path/reference safety.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for ambiguous backends, relative paths,
    /// invalid environment references, or unavailable `PostgreSQL` support.
    pub fn validate(&self) -> PulseResult<()> {
        if self.sqlite_path.is_some() && self.postgres_url_env.is_some() {
            return Err(PulseError::configuration(
                "configure only one Pulse database backend",
            ));
        }
        if let Some(path) = &self.sqlite_path
            && !path.is_absolute()
        {
            return Err(PulseError::configuration(
                "pulse.database.sqlite_path must be absolute",
            ));
        }
        if let Some(name) = &self.postgres_url_env {
            validate_environment_name(name)?;
            if !cfg!(feature = "pulse-postgres") {
                return Err(PulseError::configuration(
                    "PostgreSQL requires the pulse-postgres build feature",
                ));
            }
        }
        Ok(())
    }
}

/// One authoritative, jittered scheduler table. Values are seconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct PulseScheduleConfig {
    pub usage: u64,
    pub context: u64,
    pub tokens: u64,
    pub gemini: u64,
    pub retention: u64,
    /// Symmetric random jitter percentage applied to scheduled collection.
    pub jitter_percent: u8,
    /// Recent transcript lookback repeated by ordinary token collection.
    pub token_lookback_days: u16,
}

impl Default for PulseScheduleConfig {
    fn default() -> Self {
        Self {
            usage: 15 * 60,
            context: 2 * 60,
            tokens: 30 * 60,
            gemini: 30 * 60,
            retention: 60 * 60,
            jitter_percent: 10,
            token_lookback_days: 2,
        }
    }
}

impl PulseScheduleConfig {
    /// Validates scheduler floors and bounded jitter/lookback.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for a cadence below its provider-safe
    /// floor, jitter over 50%, or an unbounded ordinary lookback.
    pub fn validate(&self) -> PulseResult<()> {
        for (name, value, minimum) in [
            ("usage", self.usage, MIN_USAGE_SECONDS),
            ("context", self.context, MIN_CONTEXT_SECONDS),
            ("tokens", self.tokens, MIN_TOKEN_SECONDS),
            ("gemini", self.gemini, MIN_GEMINI_SECONDS),
            ("retention", self.retention, MIN_RETENTION_SECONDS),
        ] {
            if value < minimum {
                return Err(PulseError::configuration(format!(
                    "pulse.schedule.{name} must be at least {minimum} seconds"
                )));
            }
        }
        if self.jitter_percent > 50 {
            return Err(PulseError::configuration(
                "pulse.schedule.jitter_percent cannot exceed 50",
            ));
        }
        if !(1..=31).contains(&self.token_lookback_days) {
            return Err(PulseError::configuration(
                "pulse.schedule.token_lookback_days must be between 1 and 31",
            ));
        }
        Ok(())
    }
}

/// Credential behavior common to collectors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct PulseCredentialConfig {
    pub default_refresh: RefreshPolicy,
    /// Explicit opt-in to the one-token Anthropic inference fallback.
    pub anthropic_inference_fallback: bool,
    /// Detect and narrowly heal a mismatched Claude config directory.
    pub heal_config_dir: bool,
    /// Environment variable containing the Gemini OAuth application client id.
    pub gemini_oauth_client_id_env: Option<String>,
    /// Environment variable containing the Gemini OAuth application client secret.
    pub gemini_oauth_client_secret_env: Option<String>,
}

impl Default for PulseCredentialConfig {
    fn default() -> Self {
        Self {
            default_refresh: RefreshPolicy::InMemory,
            anthropic_inference_fallback: false,
            heal_config_dir: true,
            gemini_oauth_client_id_env: None,
            gemini_oauth_client_secret_env: None,
        }
    }
}

impl PulseCredentialConfig {
    fn validate(&self) -> PulseResult<()> {
        match (
            self.gemini_oauth_client_id_env.as_deref(),
            self.gemini_oauth_client_secret_env.as_deref(),
        ) {
            (Some(client_id), Some(client_secret)) => {
                validate_environment_name(client_id)?;
                validate_environment_name(client_secret)?;
                if client_id == client_secret {
                    return Err(PulseError::configuration(
                        "Gemini OAuth client id and client secret require distinct environment references",
                    ));
                }
            }
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => {
                return Err(PulseError::configuration(
                    "configure both Gemini OAuth application environment references or neither",
                ));
            }
        }
        Ok(())
    }
}

/// Bounded lifecycle settings in days.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct PulseRetentionConfig {
    pub context_days: u16,
    pub alert_days: u16,
    pub hourly_snapshots_after_days: u16,
    pub daily_snapshots_after_days: u16,
}

impl Default for PulseRetentionConfig {
    fn default() -> Self {
        Self {
            context_days: 1,
            alert_days: 180,
            hourly_snapshots_after_days: 7,
            daily_snapshots_after_days: 90,
        }
    }
}

impl PulseRetentionConfig {
    /// Validates nonzero and ordered retention periods.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when retention is disabled accidentally
    /// or daily downsampling begins before hourly downsampling.
    pub fn validate(&self) -> PulseResult<()> {
        if self.context_days == 0
            || self.alert_days == 0
            || self.hourly_snapshots_after_days == 0
            || self.daily_snapshots_after_days == 0
        {
            return Err(PulseError::configuration(
                "Pulse retention periods must be nonzero",
            ));
        }
        if self.daily_snapshots_after_days <= self.hourly_snapshots_after_days {
            return Err(PulseError::configuration(
                "daily snapshot downsampling must begin after hourly downsampling",
            ));
        }
        Ok(())
    }
}

fn validate_report_target(value: &str) -> PulseResult<bool> {
    let uri = value
        .parse::<Uri>()
        .map_err(|error| PulseError::configuration(format!("invalid pulse.report_to: {error}")))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| PulseError::configuration("pulse.report_to requires a URL scheme"))?;
    let host = uri
        .host()
        .ok_or_else(|| PulseError::configuration("pulse.report_to requires a host"))?;
    let authority = uri
        .authority()
        .ok_or_else(|| PulseError::configuration("pulse.report_to requires an authority"))?;
    if authority.as_str().contains('@')
        || uri
            .path_and_query()
            .and_then(|value| value.query())
            .is_some()
    {
        return Err(PulseError::configuration(
            "pulse.report_to cannot contain credentials or a query string",
        ));
    }
    let loopback = host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1";
    if scheme != "https" && !(scheme == "http" && loopback) {
        return Err(PulseError::configuration(
            "pulse.report_to must use HTTPS unless it targets loopback",
        ));
    }
    Ok(loopback)
}

fn validate_environment_name(value: &str) -> PulseResult<()> {
    if value.len() > 128 {
        return Err(PulseError::configuration(
            "credential environment variable name is invalid",
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table_uses_safe_capability_and_schedule_defaults() {
        let config: PulseConfig = toml::from_str("").expect("parse defaults");
        assert!(!config.collect);
        assert!(!config.serve);
        assert!(!config.receive);
        assert_eq!(config.report_to, None);
        assert_eq!(config.effective_federation_interval_seconds(), 300);
        assert_eq!(config.schedule.usage, 900);
        assert_eq!(config.schedule.context, 120);
        assert_eq!(config.schedule.tokens, 1_800);
        assert_eq!(config.schedule.gemini, 1_800);
        assert_eq!(config.schedule.retention, 3_600);
        assert_eq!(config.credentials.default_refresh, RefreshPolicy::InMemory);
        assert!(!config.credentials.anthropic_inference_fallback);
        assert!(config.credentials.gemini_oauth_client_id_env.is_none());
        assert!(config.credentials.gemini_oauth_client_secret_env.is_none());
        config.validate().expect("default is valid");
    }

    #[test]
    fn gemini_collection_requires_distinct_external_application_references() {
        let accounts = r#"
[[accounts]]
id = 1
identity = "operator@example.test"

[[accounts.profiles]]
name = "gemini"
vendor = "gemini"
config_dir = "/tmp/gemini-fixture"
"#;
        let missing: PulseConfig = toml::from_str(&format!("collect = true\n{accounts}"))
            .expect("parse missing references");
        assert!(missing.validate().is_err());

        let valid: PulseConfig = toml::from_str(&format!(
            r#"collect = true
[credentials]
gemini_oauth_client_id_env = "ATMUX_GEMINI_OAUTH_CLIENT_ID"
gemini_oauth_client_secret_env = "ATMUX_GEMINI_OAUTH_CLIENT_SECRET"
{accounts}
"#
        ))
        .expect("parse external references");
        valid.validate().expect("external references are valid");

        let reused: PulseConfig = toml::from_str(&format!(
            r#"collect = true
[credentials]
gemini_oauth_client_id_env = "ATMUX_GEMINI_OAUTH_CLIENT"
gemini_oauth_client_secret_env = "ATMUX_GEMINI_OAUTH_CLIENT"
{accounts}
"#
        ))
        .expect("parse reused reference");
        assert!(reused.validate().is_err());
    }

    #[test]
    fn receiver_requires_serving() {
        let config = PulseConfig {
            serve: false,
            receive: true,
            ..PulseConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn reporting_is_https_and_credential_reference_only() {
        let valid = PulseConfig {
            report_to: Some("https://pulse.example.test/api/v1/ingest".to_owned()),
            report_token_env: Some("ATMUX_PULSE_INGEST_TOKEN".to_owned()),
            report_node_token_env: Some("ATMUX_NODE_TOKEN".to_owned()),
            ..PulseConfig::default()
        };
        valid.validate().expect("valid remote target");

        let cleartext_remote = PulseConfig {
            report_to: Some("http://pulse.example.test/api/v1/ingest".to_owned()),
            report_token_env: Some("ATMUX_PULSE_INGEST_TOKEN".to_owned()),
            report_node_token_env: Some("ATMUX_NODE_TOKEN".to_owned()),
            ..PulseConfig::default()
        };
        assert!(cleartext_remote.validate().is_err());

        let inline_credential = PulseConfig {
            report_to: Some("https://user:secret@pulse.example.test/ingest".to_owned()),
            report_token_env: Some("ATMUX_PULSE_INGEST_TOKEN".to_owned()),
            report_node_token_env: Some("ATMUX_NODE_TOKEN".to_owned()),
            ..PulseConfig::default()
        };
        assert!(inline_credential.validate().is_err());

        let loopback = PulseConfig {
            report_to: Some("http://127.0.0.1:7345/api/v1/ingest".to_owned()),
            report_token_file: Some(PathBuf::from("/run/secrets/pulse-token")),
            ..PulseConfig::default()
        };
        loopback.validate().expect("loopback http is permitted");

        let remote_without_outer_auth = PulseConfig {
            report_to: Some("https://pulse.example.test/api/v1/ingest".to_owned()),
            report_token_env: Some("ATMUX_PULSE_INGEST_TOKEN".to_owned()),
            ..PulseConfig::default()
        };
        assert!(remote_without_outer_auth.validate().is_err());

        let reused_credential = PulseConfig {
            report_to: Some("https://pulse.example.test/api/v1/ingest".to_owned()),
            report_token_env: Some("SHARED_TOKEN".to_owned()),
            report_node_token_env: Some("SHARED_TOKEN".to_owned()),
            ..PulseConfig::default()
        };
        assert!(reused_credential.validate().is_err());
    }

    #[test]
    fn scheduler_enforces_provider_safe_floors() {
        let config = PulseConfig {
            schedule: PulseScheduleConfig {
                gemini: 29,
                ..PulseScheduleConfig::default()
            },
            ..PulseConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn federation_resync_interval_is_bounded() {
        for seconds in [29, 86_401] {
            let config = PulseConfig {
                federation_interval_seconds: Some(seconds),
                ..PulseConfig::default()
            };
            assert!(config.validate().is_err());
        }
        for seconds in [30, 300, 86_400] {
            let config = PulseConfig {
                federation_interval_seconds: Some(seconds),
                ..PulseConfig::default()
            };
            config.validate().expect("bounded interval");
        }
    }

    #[test]
    fn serialized_config_has_no_inline_secret_fields() {
        let encoded = toml::to_string(&PulseConfig::default()).expect("serialize config");
        assert!(!encoded.contains("api_key"));
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("report_token ="));
    }
}
