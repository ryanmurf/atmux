use super::{Candidate, LaunchPolicy, PoolFuture, canonical_repo};
use crate::control::ControlPlane;
use anyhow::{Result, ensure};
use serde_json::Value;
pub trait AgentPool: Send + Sync {
    fn candidates(&self) -> Result<Vec<Candidate>>;
    fn policy_available(&self, policy: &LaunchPolicy) -> bool;
    fn find(&self, query: &str) -> Result<Value>;
    fn folder<'a>(&'a self, machine: &'a str, repo: &'a str) -> PoolFuture<'a, Option<String>>;
    fn roots<'a>(&'a self, machine: &'a str) -> PoolFuture<'a, Vec<String>>;
    fn clone_repo<'a>(
        &'a self,
        machine: &'a str,
        root: &'a str,
        repo: &'a str,
    ) -> PoolFuture<'a, String>;
    fn launch<'a>(
        &'a self,
        policy: &'a LaunchPolicy,
        name: &'a str,
        folder: &'a str,
    ) -> PoolFuture<'a, Candidate>;
    fn send<'a>(&'a self, candidate: &'a Candidate, text: &'a str) -> PoolFuture<'a, ()>;
    fn audit(&self, kind: &str, key: &str, detail: Value) -> Result<()>;
}
pub struct ControlPool(pub ControlPlane);
impl AgentPool for ControlPool {
    fn candidates(&self) -> Result<Vec<Candidate>> {
        let overview = self.0.overview();
        ensure!(overview.sessions.len() <= 2000, "agent pool exceeded bound");
        overview
            .sessions
            .into_iter()
            .filter_map(|s| s.session_key.clone().map(|key| (s, key)))
            .map(|(s, key)| {
                let record = self.0.session_get(&key)?;
                let summary = self.0.agent_summary(&s.id)?;
                Ok(Candidate {
                    session_key: key,
                    pane: s.id,
                    instance_id: s.instance_id,
                    machine: s.machine,
                    name: s.name,
                    folder: record.as_ref().map_or(s.path, |r| r.cwd.clone()),
                    repo_remote: record.and_then(|r| r.project.remote),
                    status: s.status,
                    digest: summary.digest.chars().take(8000).collect(),
                })
            })
            .collect()
    }
    fn policy_available(&self, p: &LaunchPolicy) -> bool {
        self.0.launch_options().machines.iter().any(|m| {
            m.id == p.machine
                && m.online
                && m.profiles.iter().any(|profile| {
                    profile.id == p.profile_id
                        && profile.modes.iter().any(|mode| mode.id == p.mode_id)
                })
        })
    }
    fn find(&self, query: &str) -> Result<Value> {
        let query: String = query.chars().take(500).collect();
        if query.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::to_value(self.0.sessions_find(&query, 10)?)?)
    }
    fn folder<'a>(&'a self, machine: &'a str, repo: &'a str) -> PoolFuture<'a, Option<String>> {
        Box::pin(async move { self.0.find_launch_repository(machine, repo).await })
    }
    fn roots<'a>(&'a self, machine: &'a str) -> PoolFuture<'a, Vec<String>> {
        Box::pin(async move {
            Ok(self
                .0
                .browse_launch_directories(Some(machine), None)
                .await?
                .directories
                .into_iter()
                .map(|d| d.path)
                .collect())
        })
    }
    fn clone_repo<'a>(
        &'a self,
        machine: &'a str,
        root: &'a str,
        repo: &'a str,
    ) -> PoolFuture<'a, String> {
        Box::pin(async move {
            let result = self
                .0
                .clone_launch_repository(crate::control::CloneLaunchRepositoryRequest {
                    machine: Some(machine.into()),
                    directory: root.into(),
                    repository: repo.into(),
                    destination: None,
                })
                .await?;
            Ok(result.directory.path)
        })
    }
    fn launch<'a>(
        &'a self,
        p: &'a LaunchPolicy,
        name: &'a str,
        folder: &'a str,
    ) -> PoolFuture<'a, Candidate> {
        Box::pin(async move {
            self.0
                .launch(crate::control::LaunchRequest {
                    name: name.into(),
                    directory: folder.into(),
                    profile_id: p.profile_id.clone(),
                    mode_id: Some(p.mode_id.clone()),
                    machine: Some(p.machine.clone()),
                    resume_session_id: None,
                    summarize_pane_id: None,
                    memory_max_bytes: None,
                })
                .await?;
            for _ in 0..60 {
                if let Some(candidate) = self
                    .candidates()?
                    .into_iter()
                    .find(|c| c.machine == p.machine && c.name == name && c.status == "waiting")
                {
                    return Ok(candidate);
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            anyhow::bail!("launched session did not appear in registry")
        })
    }
    fn send<'a>(&'a self, c: &'a Candidate, text: &'a str) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            let live = self
                .candidates()?
                .into_iter()
                .find(|s| s.session_key == c.session_key && s.instance_id == c.instance_id)
                .ok_or_else(|| anyhow::anyhow!("assigned pane generation changed"))?;
            ensure!(live.status == "waiting", "assigned session is not idle");
            self.0
                .send_text_for_instance(&c.pane, text.into(), true, Some(c.instance_id.clone()))
                .await
        })
    }
    fn audit(&self, kind: &str, key: &str, detail: Value) -> Result<()> {
        let mut event = crate::events::AgentEvent::node_started(self.0.local_id())?;
        event.event_type = kind.into();
        event.detail = serde_json::json!({"source_key":super::request_id(key),"decision":detail});
        if let Some(session) = detail
            .get("session_key")
            .and_then(Value::as_str)
            .filter(|s| crate::tmux::valid_session_key(s))
        {
            event.session_key = session.into();
        }
        self.0.append_fleet_agent_event(event)
    }
}
/// Read-only repository discovery. Refuse an incomplete scan rather than clone over it.
pub fn find_repository(config: &crate::config::Config, repo: &str) -> Result<Option<String>> {
    let wanted =
        canonical_repo(repo).ok_or_else(|| anyhow::anyhow!("invalid repository remote"))?;
    let cache = crate::events::ProjectCache::default();
    let mut stack: Vec<_> = config.launch_roots().into_iter().map(|p| (p, 0)).collect();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut seen = 0;
    while let Some((path, depth)) = stack.pop() {
        seen += 1;
        ensure!(
            seen <= 2000 && std::time::Instant::now() < deadline,
            "repository scan exceeded bound"
        );
        if path.join(".git").exists() {
            if cache
                .get(&path)
                .remote
                .as_deref()
                .and_then(canonical_repo)
                .as_ref()
                == Some(&wanted)
            {
                return Ok(Some(path.to_string_lossy().into()));
            }
            continue;
        }
        if depth >= 4 {
            continue;
        }
        for (index, child) in std::fs::read_dir(&path)?.take(2001).enumerate() {
            ensure!(index < 2000, "repository scan exceeded bound");
            let child = child?;
            if child.file_type()?.is_dir()
                && !child.file_name().to_string_lossy().starts_with('.')
                && !["node_modules", "target", "vendor"]
                    .contains(&child.file_name().to_string_lossy().as_ref())
            {
                ensure!(stack.len() < 2000, "repository scan exceeded bound");
                stack.push((child.path(), depth + 1));
            }
        }
    }
    Ok(None)
}
