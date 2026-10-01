//! Bounded agent lifecycle events. Native hook bodies never enter the log.
mod hooks;
mod runtime;
pub mod sink;
mod spool;

pub use hooks::{configure_profiles, hook_client, inject_command};
pub use runtime::EventService;
pub use sink::RedpandaConfig;
pub use spool::{EventFilter, EventLog, EventPage, EventQuery, StoredEvent};

use crate::{
    machine::validate_machine_id,
    status::AgentKind,
    tmux::{self, Session},
};
use anyhow::{Result, bail};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

pub const SCHEMA: &str = "atmux.agent.event/v1";
pub const MAX_EVENT_BYTES: usize = 64 * 1024;
pub const MAX_PAGE_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventsConfig {
    pub inject_hooks: bool,
    pub directory: Option<PathBuf>,
    pub max_bytes: u64,
    pub segment_bytes: u64,
    pub retention_seconds: u64,
    pub redpanda: Option<RedpandaConfig>,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            inject_hooks: true,
            directory: None,
            max_bytes: 16 * 1024 * 1024,
            segment_bytes: 1024 * 1024,
            retention_seconds: 7 * 86400,
            redpanda: None,
        }
    }
}

impl EventsConfig {
    /// # Errors
    /// Rejects unbounded or contradictory retention settings.
    pub fn validate(&self, coordinator: bool) -> Result<()> {
        if !(128 * 1024..=256 * 1024 * 1024).contains(&self.max_bytes)
            || !(MAX_EVENT_BYTES as u64 + 4096..=self.max_bytes / 2).contains(&self.segment_bytes)
            || !(1..=90 * 86400).contains(&self.retention_seconds)
        {
            bail!("invalid [events] spool bounds");
        }
        if let Some(sink) = &self.redpanda {
            if !coordinator {
                bail!(
                    "[events.redpanda] requires a coordinator (coordinator_only or configured machines)"
                );
            }
            sink.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EventProject {
    pub remote: Option<String>,
    pub branch: Option<String>,
    pub root: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEvent {
    pub schema: String,
    pub id: String,
    pub time: String,
    pub machine: String,
    pub session_key: String,
    pub pane: String,
    pub instance_id: String,
    pub session_name: String,
    pub harness: String,
    pub profile: String,
    pub model: Option<String>,
    pub cwd: String,
    pub project: EventProject,
    #[serde(rename = "type")]
    pub event_type: String,
    pub reason: Option<String>,
    pub summary: Option<String>,
    pub detail: Value,
}

const TYPES: &[&str] = &[
    "agent.started",
    "agent.turn_completed",
    "agent.needs_input",
    "agent.working",
    "agent.compacted",
    "agent.summary_updated",
    "agent.renamed",
    "agent.exited",
    "session.closed",
    "session.archived",
    "session.resumed",
    "node.started",
];
const REASONS: &[&str] = &[
    "idle_prompt",
    "question",
    "permission",
    "startup_prompt",
    "plan_approval",
];

impl AgentEvent {
    /// # Errors
    /// Requires OS randomness for a boot-scoped node identity.
    pub fn node_started(machine: &str) -> Result<Self> {
        let key = tmux::new_session_key()?;
        Ok(Self {
            schema: SCHEMA.into(),
            id: tmux::new_session_key()?,
            time: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            machine: machine.into(),
            session_key: key.clone(),
            pane: format!("{machine}~node"),
            instance_id: key.clone(),
            session_name: String::new(),
            harness: "other".into(),
            profile: String::new(),
            model: None,
            cwd: String::new(),
            project: EventProject::default(),
            event_type: "node.started".into(),
            reason: None,
            summary: None,
            detail: json!({"boot_id": key, "version":env!("CARGO_PKG_VERSION")}),
        })
    }
    /// # Errors
    /// Requires an established pane session key and OS randomness.
    pub fn from_session(
        machine: &str,
        session: &Session,
        event_type: &str,
        reason: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            schema: SCHEMA.into(),
            id: tmux::new_session_key()?,
            time: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            machine: machine.into(),
            session_key: session
                .session_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("pane lacks a session key"))?,
            pane: format!("{machine}~{}", session.pane_id),
            instance_id: session.pane_identity.clone(),
            session_name: session.name.clone(),
            harness: match session.agent {
                AgentKind::Claude => "claude",
                AgentKind::Codex => "codex",
                AgentKind::Other => "other",
            }
            .into(),
            profile: session.profile.clone(),
            model: None,
            cwd: session.path.to_string_lossy().into_owned(),
            project: EventProject::default(),
            event_type: event_type.into(),
            reason: reason.map(str::to_owned),
            summary: None,
            detail: json!({}),
        })
    }

    /// # Errors
    /// Rejects malformed envelopes, unknown types/reasons and oversized data.
    pub fn validate(&self) -> Result<()> {
        if self.schema != SCHEMA
            || !tmux::valid_session_key(&self.id)
            || !tmux::valid_session_key(&self.session_key)
        {
            bail!("invalid event schema or UUIDv7");
        }
        validate_machine_id(&self.machine)?;
        let time = chrono::DateTime::parse_from_rfc3339(&self.time)?;
        if time.offset().local_minus_utc() != 0
            || !TYPES.contains(&self.event_type.as_str())
            || !["claude", "codex", "other"].contains(&self.harness.as_str())
            || !self.detail.is_object()
        {
            bail!("invalid event time, type, harness or detail");
        }
        if self.event_type == "agent.needs_input"
            && !self.reason.as_deref().is_some_and(|v| REASONS.contains(&v))
        {
            bail!("invalid needs_input reason");
        }
        if self.event_type == "agent.started"
            && !self.reason.as_deref().is_some_and(|v| {
                ["launch", "relaunch", "resume", "restore", "discovered"].contains(&v)
            })
        {
            bail!("invalid started reason");
        }
        if !self.pane.starts_with(&format!("{}~", self.machine)) {
            bail!("pane belongs to another machine");
        }
        for text in [
            &self.instance_id,
            &self.session_name,
            &self.profile,
            &self.cwd,
        ] {
            if text.len() > 4096 || text.chars().any(char::is_control) {
                bail!("invalid event attribute");
            }
        }
        if self
            .summary
            .as_ref()
            .is_some_and(|v| v.len() > 2048 || v.contains(['\n', '\r']))
        {
            bail!("invalid event summary");
        }
        if let Some(remote) = &self.project.remote
            && credential_free_remote(remote).as_ref() != Some(remote)
        {
            bail!("event remote contains credentials or is invalid");
        }
        if serde_json::to_vec(self)?.len() > MAX_EVENT_BYTES {
            bail!("event exceeds 64 KiB");
        }
        Ok(())
    }

    /// # Errors
    /// Validates before serializing; never silently truncates JSON.
    pub fn bounded_json(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }
}

/// Removes URL credentials, query tokens and fragments; converts SCP SSH URLs
/// to explicit SSH URLs without retaining a username.
#[must_use]
pub fn credential_free_remote(remote: &str) -> Option<String> {
    let remote = remote.trim();
    if remote.len() > 4096 || remote.chars().any(char::is_control) {
        return None;
    }
    let normalized = if remote.contains("://") {
        remote.to_owned()
    } else {
        let (_, host_path) = remote.split_once('@')?;
        let (host, path) = host_path.split_once(':')?;
        format!("ssh://{host}/{path}")
    };
    let mut url = url::Url::parse(&normalized).ok()?;
    if !["http", "https", "ssh", "git"].contains(&url.scheme()) || url.host_str().is_none() {
        return None;
    }
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

#[derive(Debug, Default)]
pub(crate) struct ProjectCache(Mutex<HashMap<PathBuf, (Instant, EventProject)>>);
impl ProjectCache {
    pub fn get(&self, cwd: &Path) -> EventProject {
        let mut cache = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((time, value)) = cache.get(cwd)
            && time.elapsed() < Duration::from_secs(60)
        {
            return value.clone();
        }
        let value = EventProject {
            remote: git_value(cwd, &["config", "--get", "remote.origin.url"])
                .and_then(|v| credential_free_remote(&v)),
            branch: git_value(cwd, &["symbolic-ref", "--short", "HEAD"]),
            root: git_value(cwd, &["rev-parse", "--show-toplevel"]),
        };
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(cwd.to_owned(), (Instant::now(), value.clone()));
        value
    }
}

fn git_value(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(80);
    loop {
        if let Some(status) = child.try_wait().ok()? {
            if !status.success() {
                return None;
            }
            let mut bytes = Vec::new();
            child
                .stdout
                .take()?
                .take(4097)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > 4096 {
                return None;
            }
            let text = String::from_utf8(bytes).ok()?.trim().to_owned();
            return (!text.is_empty() && !text.chars().any(char::is_control)).then_some(text);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests;
