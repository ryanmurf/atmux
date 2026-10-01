//! Opt-in coordinator intake; each item is independently triaged and routed.
#![allow(clippy::missing_errors_doc)]
mod config;
mod pool;
mod router;
mod sources;
mod store;
use crate::{
    github::GithubClient,
    herodevs::{DeviceAuth, HerodevsClient},
    llm::LlmClient,
};
use anyhow::{Result, ensure};
pub use config::{Board, IntakeConfig, LaunchPolicy};
pub(crate) use pool::find_repository as pool_lookup;
pub use pool::{AgentPool, ControlPool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, sync::Arc};
pub use store::{Store, WorkItem, WorkJob};
pub type PoolFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub session_key: String,
    pub pane: String,
    pub instance_id: String,
    pub machine: String,
    pub name: String,
    pub folder: String,
    pub repo_remote: Option<String>,
    pub status: String,
    pub digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Decision {
    Existing { session_key: String },
    Launch { policy_id: String },
    Escalate { reason: String },
}
pub struct Intake {
    pub config: IntakeConfig,
    pub ledger: HerodevsClient,
    pub github: GithubClient,
    pub triage: LlmClient,
    pub routing: LlmClient,
    pub pool: Arc<dyn AgentPool>,
    pub store: Store,
}
impl Intake {
    pub fn new(
        config: IntakeConfig,
        ledger: HerodevsClient,
        pool: Arc<dyn AgentPool>,
    ) -> Result<Self> {
        let mut store = Store::open(&config.store_dir)?;
        store
            .state
            .jobs
            .retain(|_, j| j.message.job_state.as_deref() != Some("DRY_RUN"));
        Ok(Self {
            github: GithubClient::new(config.github.clone())?,
            triage: LlmClient::new(config.triage.clone())?,
            routing: LlmClient::new(config.routing.clone())?,
            config,
            ledger,
            pool,
            store,
        })
    }
    pub fn audit(&self, kind: &str, key: &str, detail: Value) -> Result<()> {
        self.pool.audit(kind, key, detail)
    }
    pub fn check(&self) -> Result<()> {
        ensure!(!self.config.stopped(), "intake kill switch is active");
        Ok(())
    }
    pub fn llm_budget(&mut self) -> Result<()> {
        self.check()?;
        ensure!(
            self.store
                .reserve_llm(epoch(), self.config.llm_calls_per_hour)?,
            "intake LLM hourly budget exhausted"
        );
        Ok(())
    }
    pub async fn tick(&mut self) -> Result<()> {
        self.check()?;
        // Independent sources keep progressing if one provider is unavailable.
        if self.boards().await.is_err() {
            self.audit("intake.source_error", "github", json!({"source":"github"}))?;
        }
        if self.meetings().await.is_err() {
            self.audit(
                "intake.source_error",
                "meetings",
                json!({"source":"meetings"}),
            )?;
        }
        if self.slack().await.is_err() {
            self.audit("intake.source_error", "slack", json!({"source":"slack"}))?;
        }
        if self.followups().await.is_err() {
            self.audit("intake.source_error", "atmux", json!({"source":"atmux"}))?;
        }
        self.sync_ledger().await?;
        let keys: Vec<_> = self
            .store
            .state
            .jobs
            .iter()
            .filter(|(_, j)| {
                !j.dispatched
                    && j.blocked.is_none()
                    && !j.item.source_done
                    && [
                        Some("PENDING"),
                        Some("DRY_RUN"),
                        Some("CLAIMED"),
                        Some("IN_PROGRESS"),
                    ]
                    .contains(&j.message.job_state.as_deref())
            })
            .take(self.config.batch_size)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            self.check()?;
            if self.route(&key).await.is_err() {
                self.audit("intake.route_error", &key, json!({"retry":true}))?;
            }
        }
        Ok(())
    }
}
#[must_use]
pub fn epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub use crate::herodevs::client_request_id as request_id;
#[must_use]
pub fn canonical_repo(remote: &str) -> Option<String> {
    if remote.len() > 4096 || remote.chars().any(char::is_control) {
        return None;
    }
    let normalized = remote
        .strip_prefix("git@")
        .filter(|_| !remote.contains("://"))
        .and_then(|r| r.split_once(':'))
        .map_or_else(
            || remote.to_owned(),
            |(host, path)| format!("ssh://{host}/{path}"),
        );
    let mut url = url::Url::parse(&normalized).ok()?;
    if !["https", "http", "ssh", "git"].contains(&url.scheme())
        || url.host_str().is_none()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.username().is_empty() || url.scheme() == "ssh" && url.username() == "git")
    {
        return None;
    }
    url.set_username("").ok()?;
    if url.host_str() == Some("github.com") && url.scheme() == "ssh" {
        return Some(format!(
            "https://github.com{}",
            url.path().trim_end_matches('/').trim_end_matches(".git")
        ));
    }
    Some(
        url.as_str()
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .to_owned(),
    )
}

pub fn spawn(control: crate::control::ControlPlane, config: crate::config::Config) -> Result<()> {
    if !config.intake.enabled {
        return Ok(());
    }
    config.intake.validate(&config)?;
    let auth = Arc::new(DeviceAuth::new(config.herodevs.auth.clone())?);
    let ledger = HerodevsClient::new(config.herodevs.clone(), auth)?;
    let mut intake = Intake::new(config.intake, ledger, Arc::new(ControlPool(control)))?;
    tokio::spawn(async move {
        loop {
            if !intake.config.stopped() {
                let _ = intake.tick().await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(intake.config.poll_seconds)).await;
        }
    });
    Ok(())
}
#[derive(Serialize)]
pub struct WorkView {
    pub enabled: bool,
    pub dry_run: bool,
    pub stopped: bool,
    pub jobs: Vec<WorkJob>,
}
pub fn work(config: &IntakeConfig) -> Result<WorkView> {
    let state = if config.enabled {
        store::read(&config.store_dir)?
    } else {
        store::State::default()
    };
    let mut jobs: Vec<_> = state.jobs.into_values().collect();
    jobs.sort_by_key(|job| std::cmp::Reverse(job.message.ordinal));
    jobs.truncate(500);
    Ok(WorkView {
        enabled: config.enabled,
        dry_run: config.dry_run,
        stopped: config.stopped(),
        jobs,
    })
}
#[cfg(test)]
mod tests;
