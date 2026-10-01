use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntakeConfig {
    pub enabled: bool,
    pub dry_run: bool,
    pub kill_switch_file: PathBuf,
    pub store_dir: PathBuf,
    pub poll_seconds: u64,
    pub batch_size: usize,
    pub github: crate::github::GithubConfig,
    pub triage: crate::llm::LlmConfig,
    pub routing: crate::llm::LlmConfig,
    pub boards: Vec<Board>,
    pub meetings_channel: Option<String>,
    pub slack_channel: Option<String>,
    pub slack_jobs_channel: Option<String>,
    pub followups_channel: Option<String>,
    pub escalation_channel: Option<String>,
    pub owner_names: Vec<String>,
    pub policies: Vec<LaunchPolicy>,
    pub allowed_repo_prefixes: Vec<String>,
    pub new_sessions_per_hour: u64,
    pub new_sessions_per_day: u64,
    pub sessions_per_machine: usize,
    pub llm_calls_per_hour: u64,
}
impl Default for IntakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            kill_switch_file: PathBuf::new(),
            store_dir: PathBuf::new(),
            poll_seconds: 60,
            batch_size: 10,
            github: crate::github::GithubConfig::default(),
            triage: crate::llm::LlmConfig::default(),
            routing: crate::llm::LlmConfig {
                endpoint: "http://192.168.0.124:8094/v1".into(),
                model: "qwen3-27b".into(),
                ..crate::llm::LlmConfig::default()
            },
            boards: vec![],
            meetings_channel: None,
            slack_channel: None,
            slack_jobs_channel: None,
            followups_channel: None,
            escalation_channel: None,
            owner_names: vec!["Ryan".into(), "ryanmurf".into()],
            policies: vec![],
            allowed_repo_prefixes: vec!["https://github.com/neverendingsupport/".into()],
            new_sessions_per_hour: 4,
            new_sessions_per_day: 20,
            sessions_per_machine: 4,
            llm_calls_per_hour: 120,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Board {
    pub number: u32,
    pub channel_id: String,
    #[serde(default = "org")]
    pub org: String,
    #[serde(default = "done")]
    pub done_statuses: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
}
fn org() -> String {
    "neverendingsupport".into()
}
fn done() -> Vec<String> {
    vec!["Done".into()]
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchPolicy {
    pub id: String,
    pub machine: String,
    pub project_root: String,
    #[serde(default = "profile")]
    pub profile_id: String,
    #[serde(default = "mode")]
    pub mode_id: String,
    #[serde(default)]
    pub job_types: Vec<String>,
    #[serde(default)]
    pub repo_remote: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub allow_clone: bool,
}
fn profile() -> String {
    "profile-0".into()
}
fn mode() -> String {
    "sol61-xhigh".into()
}
impl IntakeConfig {
    #[must_use]
    pub fn stopped(&self) -> bool {
        !self.enabled || self.kill_switch_file.exists()
    }
    pub fn validate(&self, config: &crate::config::Config) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        ensure!(
            config.node.coordinator_only || !config.machines.is_empty() || config.discovery.enabled,
            "intake requires a coordinator"
        );
        ensure!(
            config.registry.enabled && config.events.is_some(),
            "intake requires registry and fleet events"
        );
        ensure!(
            self.store_dir.is_absolute() && self.kill_switch_file.is_absolute(),
            "intake requires absolute store and kill switch paths"
        );
        ensure!(
            (1..=100).contains(&self.batch_size)
                && (5..=3600).contains(&self.poll_seconds)
                && self.policies.len() <= 32
                && self.boards.len() <= 8
                && !self.owner_names.is_empty(),
            "invalid intake scheduling bounds"
        );
        ensure!(
            self.new_sessions_per_hour <= 100
                && self.new_sessions_per_day <= 1000
                && self.sessions_per_machine <= 100
                && self.llm_calls_per_hour <= 10000,
            "invalid intake budgets"
        );
        self.triage.validate()?;
        self.routing.validate()?;
        config.herodevs.auth.validate()?;
        crate::github::GithubClient::new(self.github.clone())?;
        ensure!(
            self.boards.is_empty() || self.github.token_file.is_absolute(),
            "GitHub token file required for boards"
        );
        ensure!(
            self.routing.api_key_file.is_some(),
            "routing gateway requires a key file"
        );
        ensure!(
            self.owner_names.len() <= 32
                && self
                    .owner_names
                    .iter()
                    .all(|n| !n.trim().is_empty() && n.len() <= 200),
            "invalid intake owner names"
        );
        ensure!(
            self.allowed_repo_prefixes.len() <= 32
                && self
                    .allowed_repo_prefixes
                    .iter()
                    .all(|prefix| prefix.ends_with('/') && super::canonical_repo(prefix).is_some()),
            "invalid repository allowlist"
        );
        let mut ids = std::collections::HashSet::new();
        for policy in &self.policies {
            ensure!(ids.insert(&policy.id), "duplicate launch policy id");
            policy.validate()?;
        }
        for id in self.boards.iter().map(|b| &b.channel_id).chain(
            [
                &self.meetings_channel,
                &self.slack_channel,
                &self.slack_jobs_channel,
                &self.followups_channel,
                &self.escalation_channel,
            ]
            .into_iter()
            .flatten(),
        ) {
            ensure!(
                crate::session_search::valid_tenant(id),
                "channel id must be a UUID"
            );
        }
        Ok(())
    }
}

impl LaunchPolicy {
    fn validate(&self) -> Result<()> {
        crate::machine::validate_machine_id(&self.machine)?;
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 100
                && std::path::Path::new(&self.project_root).is_absolute()
                && !self.profile_id.is_empty()
                && !self.mode_id.is_empty(),
            "invalid launch policy"
        );
        ensure!(
            self.repo_remote
                .as_ref()
                .is_none_or(|r| super::canonical_repo(r).is_some()),
            "invalid policy repository"
        );
        if let Some(folder) = &self.folder {
            ensure!(
                std::path::Path::new(folder).starts_with(&self.project_root),
                "policy folder outside root"
            );
        }

        Ok(())
    }
}
