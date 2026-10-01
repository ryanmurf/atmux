use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Continue,
    Answer,
    Escalate,
    Complete,
    ProjectStatus,
    Close,
    Nudge,
    Digest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SupervisorConfig {
    pub enabled: bool,
    pub dry_run: bool,
    /// Creating this file stops all external effects, including model calls.
    pub kill_switch: Option<PathBuf>,
    pub store_dir: Option<PathBuf>,
    pub dashboard_url: String,
    pub ryan_channel: String,
    pub slack_channel: String,
    pub job_channels: Vec<String>,
    pub done_status: String,
    pub allow_actions: BTreeSet<Action>,
    pub poll_seconds: u64,
    pub quiet_seconds: u64,
    pub prompt_interval_seconds: u64,
    pub stall_seconds: u64,
    pub nudge_grace_seconds: u64,
    /// Disabled unless set; first observe with `dry_run` enabled.
    pub orphan_archive_seconds: Option<u64>,
    pub digest_hour_utc: u32,
    pub llm_calls_per_hour: u64,
    pub actions_per_hour: u64,
    pub sessions_per_machine: usize,
}
impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            kill_switch: None,
            store_dir: None,
            dashboard_url: String::new(),
            ryan_channel: "ryan-tron".into(),
            slack_channel: String::new(),
            job_channels: Vec::new(),
            done_status: "Done".into(),
            allow_actions: [
                Action::Continue,
                Action::Answer,
                Action::Escalate,
                Action::Complete,
                Action::ProjectStatus,
                Action::Close,
                Action::Nudge,
                Action::Digest,
            ]
            .into(),
            poll_seconds: 15,
            quiet_seconds: 120,
            prompt_interval_seconds: 60,
            stall_seconds: 1800,
            nudge_grace_seconds: 600,
            orphan_archive_seconds: None,
            digest_hour_utc: 18,
            llm_calls_per_hour: 120,
            actions_per_hour: 240,
            sessions_per_machine: 12,
        }
    }
}
impl SupervisorConfig {
    /// # Errors
    /// Rejects missing destinations and unbounded scheduling or storage policy.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let url = url::Url::parse(&self.dashboard_url)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || self.dashboard_url.len() > 2048
        {
            bail!("supervisor dashboard_url must be a credential-free HTTPS URL");
        }
        for value in [&self.ryan_channel, &self.slack_channel, &self.done_status] {
            if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
                bail!("supervisor destinations and done_status must be configured");
            }
        }
        if self.job_channels.is_empty()
            || self.job_channels.len() > 16
            || self
                .job_channels
                .iter()
                .any(|v| v.is_empty() || v.len() > 200 || v.chars().any(char::is_control))
            || !(1..=60).contains(&self.poll_seconds)
            || !(1..=86400).contains(&self.quiet_seconds)
            || !(1..=3600).contains(&self.prompt_interval_seconds)
            || !(60..=604_800).contains(&self.stall_seconds)
            || !(1..=86400).contains(&self.nudge_grace_seconds)
            || self
                .orphan_archive_seconds
                .is_some_and(|s| !(3600..=2_592_000).contains(&s))
            || self.digest_hour_utc > 23
            || !(1..=10000).contains(&self.llm_calls_per_hour)
            || !(1..=10000).contains(&self.actions_per_hour)
            || !(1..=256).contains(&self.sessions_per_machine)
            || self.store_dir.as_ref().is_some_and(|p| !p.is_absolute())
            || self.kill_switch.as_ref().is_none_or(|p| !p.is_absolute())
        {
            bail!("invalid [supervisor] bounds; an absolute kill_switch is required");
        }
        Ok(())
    }
    pub(crate) fn stopped(&self) -> bool {
        !self.enabled
            || self
                .kill_switch
                .as_ref()
                .is_some_and(|p| p.try_exists().unwrap_or(true))
    }
}
