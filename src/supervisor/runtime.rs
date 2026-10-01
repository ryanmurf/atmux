use super::{
    Action, Classification, Completion, Job, PullRequest, SupervisorConfig, classify, completion,
    permission_allowed,
    store::{Claim, Finished, Observation, Store},
    verify,
};
use crate::{control::SessionSummary, events::AgentEvent};
use anyhow::{Result, bail};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::{collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, sync::Arc};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
#[derive(Clone, Debug, Serialize)]
pub struct Context {
    pub session: SessionSummary,
    pub summary: String,
    pub last_turn: String,
    pub last_user: String,
    pub prompt: String,
    pub conversation_available: bool,
}
pub trait Agents: Send + Sync {
    fn sessions(&self) -> Vec<SessionSummary>;
    /// Current generation-bound attention, used to reconcile rate-limited or
    /// missed events. Pure fixtures can omit this owner-derived signal.
    fn attention(&self, _session: &SessionSummary) -> Option<String> {
        None
    }
    fn context<'a>(&'a self, session: &'a SessionSummary) -> BoxFuture<'a, Context>;
    /// Must revalidate generation and output hash at the owner mutation boundary.
    fn send<'a>(&'a self, session: &'a SessionSummary, text: &'a str) -> BoxFuture<'a, ()>;
    /// Must revalidate idle status, key, generation, hash and sole pane at owner.
    fn close<'a>(&'a self, session: &'a SessionSummary) -> BoxFuture<'a, ()>;
}
pub trait Platform: Send + Sync {
    fn jobs(&self) -> BoxFuture<'_, Vec<Job>>;
    fn complete<'a>(&'a self, job: &'a Job, outcome: &'a str, key: &'a str) -> BoxFuture<'a, ()>;
    fn blocked<'a>(&'a self, job: &'a Job, reason: &'a str) -> BoxFuture<'a, ()>;
    fn project_status<'a>(&'a self, job: &'a Job, status: &'a str) -> BoxFuture<'a, ()>;
    fn pull_request<'a>(&'a self, url: &'a str) -> BoxFuture<'a, PullRequest>;
    /// Notify both configured destinations; use key for idempotent channel posts.
    fn notify<'a>(&'a self, message: &'a str, key: &'a str) -> BoxFuture<'a, ()>;
}
pub trait Model: Send + Sync {
    fn classify<'a>(
        &'a self,
        context: &'a Context,
        job: &'a Job,
        reason: &'a str,
    ) -> BoxFuture<'a, String>;
}
pub trait Audit: Send + Sync {
    /// # Errors
    /// Rejects invalid audit events or unavailable durable storage.
    fn decision(
        &self,
        session: Option<&SessionSummary>,
        action: &str,
        detail: serde_json::Value,
    ) -> Result<()>;
}
/// One serialized coordinator loop. The lock and persisted action claims prevent
/// concurrent workers and replay after crashes from answering the same prompt.
pub struct Supervisor {
    pub config: SupervisorConfig,
    store: Store,
    agents: Arc<dyn Agents>,
    platform: Arc<dyn Platform>,
    model: Arc<dyn Model>,
    audit: Arc<dyn Audit>,
}
impl Supervisor {
    /// # Errors
    /// Requires valid policy and a private, exclusive, bounded durable store.
    pub fn open(
        config: SupervisorConfig,
        directory: PathBuf,
        agents: Arc<dyn Agents>,
        platform: Arc<dyn Platform>,
        model: Arc<dyn Model>,
        audit: Arc<dyn Audit>,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            store: Store::open(directory)?,
            agents,
            platform,
            model,
            audit,
        })
    }
    #[must_use]
    pub fn cursor(&self) -> Option<String> {
        self.store.state.cursor.clone()
    }
    /// # Errors
    /// Propagates failed durable cursor writes.
    pub fn checkpoint(&mut self, cursor: String) -> Result<()> {
        crate::events::EventQuery {
            after: Some(cursor.clone()),
            ..crate::events::EventQuery::default()
        }
        .validate()?;
        self.store.state.cursor = Some(cursor);
        self.store.save()
    }
    fn live(&self, session: &SessionSummary) -> bool {
        let sessions = self.agents.sessions();
        !self.config.stopped()
            && sessions
                .iter()
                .filter(|s| s.session_key == session.session_key)
                .count()
                == 1
            && sessions.iter().any(|s| {
                s.id == session.id
                    && s.session_key == session.session_key
                    && s.instance_id == session.instance_id
                    && s.content_hash == session.content_hash
                    && s.status == session.status
            })
    }
    fn observe(&mut self, session: &SessionSummary, now: u64) {
        let Some(key) = &session.session_key else {
            return;
        };
        let obs = self.store.state.sessions.entry(key.clone()).or_default();
        if obs.instance != session.instance_id
            || obs.hash != session.content_hash
            || obs.status != session.status
        {
            let echo = obs.nudge_echo_pending
                && obs.instance == session.instance_id
                && obs.status == session.status;
            *obs = Observation {
                instance: session.instance_id.clone(),
                hash: session.content_hash.clone(),
                status: session.status.clone(),
                changed_at: if echo { obs.changed_at } else { now },
                nudged_at: if echo { obs.nudged_at } else { None },
                answered_at: obs.answered_at,
                notified_at: obs.notified_at,
                ..Observation::default()
            };
        }
    }
    fn decision(&self, session: Option<&SessionSummary>, action: &str, reason: &str) -> Result<()> {
        self.audit.decision(
            session,
            action,
            serde_json::json!({"reason": reason, "dry_run": self.config.dry_run}),
        )
    }
    fn reserve(
        &mut self,
        session: Option<&SessionSummary>,
        action: Action,
        key: &str,
        now: u64,
    ) -> Result<bool> {
        if self.config.stopped() {
            return Ok(false);
        }
        self.store.budget(now);
        let claim = if self.config.dry_run {
            format!("dry:{key}")
        } else {
            key.into()
        };
        if self.store.state.claims.contains_key(&claim) {
            return Ok(false);
        }
        let allowed = self.config.allow_actions.contains(&action)
            && self.store.state.actions < self.config.actions_per_hour;
        self.audit.decision(
            session,
            "decision",
            serde_json::json!({"action":action,"key":key,
            "allowed":allowed,"dry_run":self.config.dry_run}),
        )?;
        if !allowed {
            return Ok(false);
        }
        // Dry-run claims are separate, so enabling actions still processes them.
        self.store.state.claims.insert(
            claim,
            Claim {
                at: now,
                session_key: session.and_then(|s| s.session_key.clone()),
                instance: session.map(|s| s.instance_id.clone()),
            },
        );
        if !self.config.dry_run {
            self.store.state.actions += 1;
            self.store.totals(now).actions += 1;
        }
        self.store.save()?;
        Ok(!self.config.dry_run && !self.config.stopped())
    }
    async fn escalate(
        &mut self,
        session: &SessionSummary,
        message: &str,
        key: &str,
        now: u64,
    ) -> Result<()> {
        if session
            .session_key
            .as_ref()
            .and_then(|k| self.store.state.sessions.get(k))
            .and_then(|o| o.notified_at)
            .is_some_and(|t| now.saturating_sub(t) < self.config.prompt_interval_seconds)
        {
            return Ok(());
        }
        if !self.reserve(
            Some(session),
            Action::Escalate,
            &format!("escalate:{key}"),
            now,
        )? {
            return Ok(());
        }
        if let Some(obs) = session
            .session_key
            .as_ref()
            .and_then(|k| self.store.state.sessions.get_mut(k))
        {
            obs.notified_at = Some(now);
            self.store.save()?;
        }
        let mut url = url::Url::parse(&self.config.dashboard_url)?;
        url.query_pairs_mut().append_pair("session", &session.id);
        let message = format!(
            "atmux supervisor: {} on {}\n{}\n{}",
            session.name,
            session.machine,
            bounded(message, 3000),
            url
        );
        let result = self.platform.notify(&message, key).await;
        self.decision(
            Some(session),
            if result.is_ok() { "escalated" } else { "error" },
            if result.is_ok() {
                "Ryan notified through channel and Slack"
            } else {
                "notification failed; inspect platform connectivity"
            },
        )?;
        self.store.totals(now).blocked += 1;
        self.store.save()?;
        result
    }
    /// Consumes one fleet event. Stale/replayed events cannot mutate a new pane.
    /// # Errors
    /// Propagates durable/audit or platform failures; callers must retain cursor.
    #[allow(clippy::too_many_lines)] // Keep fleet identity, ledger and conversation validation together.
    pub async fn event(&mut self, event: &AgentEvent, now: u64) -> Result<()> {
        if self.config.stopped() {
            return Ok(());
        }
        if event.event_type == "session.archived" {
            let archive_key = format!("{}:{}", event.session_key, event.instance_id);
            let requested = self.store.state.pending_archives.remove(&archive_key);
            if requested {
                self.store.totals(now).archived += 1;
            }
            self.store.state.claims.retain(|_, c| {
                c.session_key.as_ref() != Some(&event.session_key)
                    || c.instance.as_ref() != Some(&event.instance_id)
            });
            if self
                .store
                .state
                .sessions
                .get(&event.session_key)
                .is_some_and(|o| o.instance == event.instance_id)
            {
                self.store.state.sessions.remove(&event.session_key);
            }
            if self
                .store
                .state
                .finished
                .get(&event.session_key)
                .is_some_and(|f| f.instance == event.instance_id)
            {
                self.store.state.finished.remove(&event.session_key);
            }
            self.store.save()?;
            if requested {
                self.audit.decision(None,"archived",serde_json::json!({"session_key":event.session_key,"reason":"owner registry confirmed archive"}))?;
            }
            return Ok(());
        }
        let sessions = self.agents.sessions();
        let Some(session) = sessions.iter().find(|s| {
            s.session_key.as_deref() == Some(&event.session_key)
                && s.instance_id == event.instance_id
                && s.machine == event.machine
                && format!("{}~{}", s.machine, s.pane_id) == event.pane
        }) else {
            return Ok(());
        };
        if sessions
            .iter()
            .filter(|s| s.machine == session.machine)
            .count()
            > self.config.sessions_per_machine
        {
            self.escalate(
                session,
                "Machine session budget exceeded; automatic prompt handling suspended",
                &format!("machine-budget:{}:{}", session.machine, now / 3600),
                now,
            )
            .await?;
            return Ok(());
        }
        if sessions
            .iter()
            .filter(|s| s.session_key == session.session_key)
            .count()
            != 1
        {
            return Ok(());
        }
        self.observe(session, now);
        if !matches!(
            event.event_type.as_str(),
            "agent.needs_input" | "agent.turn_completed"
        ) {
            self.store.save()?;
            return Ok(());
        }
        if session.status != "waiting" {
            return Ok(());
        }
        let jobs = self.platform.jobs().await?;
        if jobs.len() > 512 {
            bail!("ledger job scan exceeds bounds");
        }
        let matches: Vec<_> = jobs
            .iter()
            .filter(|job| job.session_key == event.session_key && job.active())
            .collect();
        if matches.len() != 1 {
            if matches.len() > 1 {
                self.escalate(
                    session,
                    "Multiple open jobs reference this session",
                    &event.id,
                    now,
                )
                .await?;
            }
            return Ok(());
        }
        let job = matches[0];
        self.handle_job(
            session,
            job,
            &event.event_type,
            event.reason.as_deref().unwrap_or("idle_prompt"),
            now,
        )
        .await
    }
    async fn handle_job(
        &mut self,
        session: &SessionSummary,
        job: &Job,
        event_type: &str,
        reason: &str,
        now: u64,
    ) -> Result<()> {
        let context = self.agents.context(session).await?;
        if !self.live(session) || context.session != *session {
            return Ok(());
        }
        // Discovery can emit idle_prompt before a native permission hook. Use
        // current attention and visible permission evidence, never stale reason.
        let current = self.agents.attention(session);
        let prompt_lower = context.prompt.to_lowercase();
        let visible_permission = prompt_lower.contains("allow")
            && (prompt_lower.contains("command:") || prompt_lower.contains("permission"));
        let current_reason = current.as_deref().unwrap_or(reason);
        let reason = if matches!(current_reason, "startup_prompt" | "plan_approval") {
            current_reason
        } else if visible_permission {
            "permission"
        } else {
            current_reason
        };
        if reason == "startup_prompt" {
            return self.decision(
                Some(session),
                "startup",
                "startup responder owns this prompt",
            );
        }
        let fingerprint = hash(&format!(
            "{}:{}:{}:{}:{}",
            job.id, session.instance_id, reason, context.last_turn, context.last_user
        ));
        if context.conversation_available
            && let Some(done) = completion(&context.last_turn, &job.id)
        {
            return self.finish(session, job, done, &fingerprint, now).await;
        }
        if event_type == "agent.turn_completed" {
            return Ok(());
        }
        self.needs_input(&context, job, reason, &fingerprint, now)
            .await
    }
    #[allow(clippy::too_many_lines)] // Keep validation, reservation and delivery in one auditable sequence.
    async fn needs_input(
        &mut self,
        context: &Context,
        job: &Job,
        reason: &str,
        fingerprint: &str,
        now: u64,
    ) -> Result<()> {
        let session = &context.session;
        if reason == "startup_prompt" {
            return self.decision(
                Some(session),
                "startup",
                "startup responder owns this prompt",
            );
        }
        if self.store.state.finished.contains_key(&job.session_key) {
            return Ok(());
        }
        let key = format!("prompt:{fingerprint}");
        let effective_key = if self.config.dry_run {
            format!("dry:{key}")
        } else {
            key.clone()
        };
        if self.store.state.claims.contains_key(&effective_key) {
            return Ok(());
        }
        let obs = &self.store.state.sessions[&job.session_key];
        if obs
            .answered_at
            .is_some_and(|t| now.saturating_sub(t) < self.config.prompt_interval_seconds)
        {
            return Ok(());
        }
        let model_key = format!("model:{effective_key}");
        if self.store.state.claims.contains_key(&model_key) {
            return self
                .escalate(
                    session,
                    "Classification was already attempted; inspect the prompt before retrying",
                    fingerprint,
                    now,
                )
                .await;
        }
        if !context.conversation_available
            || reason == "plan_approval"
            || super::policy::unsafe_prompt(&context.prompt)
            || super::policy::unsafe_prompt(&context.last_turn)
            || (reason == "permission" && !permission_allowed(job, &session.path, &context.prompt))
        {
            return self
                .escalate(
                    session,
                    &format!("Prompt ({reason}) requires review:\n{}", context.prompt),
                    fingerprint,
                    now,
                )
                .await;
        }
        self.store.budget(now);
        if self.store.state.model_calls >= self.config.llm_calls_per_hour {
            return self
                .escalate(session, "Hourly Qwen budget exhausted", fingerprint, now)
                .await;
        }
        self.store.state.model_calls += 1;
        self.store.state.claims.insert(
            model_key,
            Claim {
                at: now,
                session_key: session.session_key.clone(),
                instance: Some(session.instance_id.clone()),
            },
        );
        self.store.totals(now).model_calls += 1;
        self.store.save()?;
        self.decision(
            Some(session),
            "classified",
            "bounded classification requested",
        )?;
        let classification = self
            .model
            .classify(context, job, reason)
            .await
            .and_then(|raw| classify(&raw));
        if !self.live(session) {
            return Ok(());
        }
        let (action, text) = match classification {
            Ok(Classification::Continue) => (
                Action::Continue,
                if reason == "permission" {
                    "y".into()
                } else {
                    "Continue working toward the assigned job's completion criteria. Stay within its scope and report JOB DONE or JOB BLOCKED with the job id.".into()
                },
            ),
            Ok(Classification::Answer { fact }) if reason == "question" => (
                Action::Answer,
                format!("Job record: {}", bounded(job.fact(fact), 1500)),
            ),
            Ok(Classification::Startup) if reason == "startup_prompt" => return Ok(()),
            _ => {
                return self
                    .escalate(
                        session,
                        &format!("Prompt ({reason}) requires review:\n{}", context.prompt),
                        fingerprint,
                        now,
                    )
                    .await;
            }
        };
        if self.reserve(Some(session), action, &key, now)? && self.live(session) {
            self.store
                .state
                .sessions
                .get_mut(&job.session_key)
                .expect("observed session")
                .answered_at = Some(now);
            self.store.save()?;
            if self.agents.send(session, &text).await.is_err() {
                self.escalate(
                    session,
                    "Prompt answer delivery was ambiguous; inspect pane before retrying",
                    fingerprint,
                    now,
                )
                .await?;
            } else {
                self.decision(
                    Some(session),
                    "answered",
                    "generation-bound prompt answer delivered",
                )?;
            }
        }
        Ok(())
    }
    async fn finish(
        &mut self,
        session: &SessionSummary,
        job: &Job,
        done: Completion,
        fingerprint: &str,
        now: u64,
    ) -> Result<()> {
        let Completion::Done { outcome, pr_url } = done else {
            let Completion::Blocked(why) = done else {
                unreachable!()
            };
            return self
                .block(
                    session,
                    job,
                    &format!("JOB BLOCKED {}: {why}", job.id),
                    fingerprint,
                    now,
                )
                .await;
        };
        if self.store.state.finished.contains_key(&job.session_key) {
            return Ok(());
        }
        let pr = if let Some(url) = &pr_url {
            self.platform.pull_request(url).await.ok()
        } else {
            None
        };
        if let Err(error) = verify(job, &outcome, pr_url.as_deref(), pr.as_ref()) {
            return self
                .block(
                    session,
                    job,
                    &format!("JOB DONE {} failed verification: {error}", job.id),
                    fingerprint,
                    now,
                )
                .await;
        }
        if !self.live(session) {
            return Ok(());
        }
        if self.reserve(
            Some(session),
            Action::Complete,
            &format!("complete:{}", job.id),
            now,
        )? {
            // Persist pending before touching the ledger. Ambiguous completion
            // never implies success and will never close the session.
            self.store.state.finished.insert(
                job.session_key.clone(),
                Finished {
                    job: job.id.clone(),
                    ledger: job.clone(),
                    completed: false,
                    project_updated: job.project_item_id.is_none(),
                    hash: session.content_hash.clone(),
                    instance: session.instance_id.clone(),
                    quiet_since: now,
                },
            );
            self.store.save()?;
            if self
                .platform
                .complete(job, &outcome, &format!("supervisor-complete-{}", job.id))
                .await
                .is_err()
            {
                return self
                    .escalate(
                        session,
                        "Job completion failed or was ambiguous; session kept open",
                        fingerprint,
                        now,
                    )
                    .await;
            }
            self.store
                .state
                .finished
                .get_mut(&job.session_key)
                .expect("pending completion")
                .completed = true;
            self.store.totals(now).completed += 1;
            self.store.save()?;
            self.decision(
                Some(session),
                "completed",
                "completion criteria verified and job completed",
            )?;
        }
        Ok(())
    }
    async fn block(
        &mut self,
        session: &SessionSummary,
        job: &Job,
        reason: &str,
        fingerprint: &str,
        now: u64,
    ) -> Result<()> {
        if self.reserve(
            Some(session),
            Action::Escalate,
            &format!("blocked:{}:{fingerprint}", job.id),
            now,
        )? {
            if self.platform.blocked(job, reason).await.is_err() {
                self.decision(
                    Some(session),
                    "error",
                    "ledger escalation failed; manual reconciliation required",
                )?;
            } else {
                self.decision(Some(session), "blocked", "job escalated in ledger")?;
            }
        }
        self.escalate(session, reason, fingerprint, now).await
    }
    /// Periodic housekeeping: completion saga, quiet closes, stalls and digest.
    /// # Errors
    /// Propagates unavailable ledger or failed durable effects.
    pub async fn tick(&mut self, now: u64) -> Result<()> {
        if self.config.stopped() {
            return Ok(());
        }
        let jobs = self.platform.jobs().await?;
        if jobs.len() > 512 {
            bail!("ledger job scan exceeds bounds");
        }
        let sessions = self.agents.sessions();
        if sessions.len() > 512 {
            bail!("supervisor session scan exceeds bounds");
        }
        let mut machines = BTreeMap::<String, usize>::new();
        for session in &sessions {
            *machines.entry(session.machine.clone()).or_default() += 1;
        }
        for session in &sessions {
            let Some(key) = &session.session_key else {
                continue;
            };
            self.observe(session, now);
            if let Some(finished) = self.store.state.finished.get(key).cloned() {
                self.close_finished(session, &finished, now).await?;
                continue;
            }
            if machines[&session.machine] > self.config.sessions_per_machine {
                self.escalate(
                    session,
                    "Machine session budget exceeded; intake must stop launching",
                    &format!("machine-budget:{}:{}", session.machine, now / 3600),
                    now,
                )
                .await?;
                continue;
            }
            let assigned: Vec<_> = jobs
                .iter()
                .filter(|j| &j.session_key == key && j.active())
                .collect();
            if session.status == "waiting"
                && assigned.len() == 1
                && let Some(reason) = self.agents.attention(session)
            {
                self.handle_job(session, assigned[0], "agent.needs_input", &reason, now)
                    .await?;
            }
            self.stall(session, &jobs, now).await?;
        }
        self.digest(&jobs, now).await?;
        self.store.save()
    }
    async fn close_finished(
        &mut self,
        session: &SessionSummary,
        finished: &Finished,
        now: u64,
    ) -> Result<()> {
        let key = session.session_key.as_ref().expect("stable session");
        if !finished.completed {
            return self
                .escalate(
                    session,
                    "Completion was interrupted; reconcile the ledger before closing",
                    &format!("pending:{}", finished.job),
                    now,
                )
                .await;
        }
        if !finished.project_updated {
            let job = &finished.ledger;
            if self.reserve(
                Some(session),
                Action::ProjectStatus,
                &format!("project:{}", job.id),
                now,
            )? {
                if self
                    .platform
                    .project_status(job, &self.config.done_status)
                    .await
                    .is_err()
                {
                    return self
                        .escalate(
                            session,
                            "GitHub project status update failed; session kept open",
                            &format!("project:{}", job.id),
                            now,
                        )
                        .await;
                }
                self.store
                    .state
                    .finished
                    .get_mut(key)
                    .expect("finished session")
                    .project_updated = true;
                self.store.save()?;
                self.decision(
                    Some(session),
                    "project_updated",
                    "configured GitHub status applied",
                )?;
            }
            return Ok(());
        }
        if session.instance_id != finished.instance {
            return Ok(());
        }
        if session.content_hash != finished.hash || session.status != "waiting" {
            let pending = self
                .store
                .state
                .finished
                .get_mut(key)
                .expect("finished session");
            pending.hash.clone_from(&session.content_hash);
            pending.quiet_since = now;
            return Ok(());
        }
        if now.saturating_sub(finished.quiet_since) < self.config.quiet_seconds
            || session.attached
            || session.windows != 1
        {
            return Ok(());
        }
        self.archive(
            session,
            &format!("close:{}:{}", finished.job, session.content_hash),
            now,
        )
        .await
    }
    async fn archive(&mut self, session: &SessionSummary, key: &str, now: u64) -> Result<()> {
        if self.reserve(Some(session), Action::Close, key, now)? && self.live(session) {
            self.store.state.pending_archives.insert(format!(
                "{}:{}",
                session.session_key.as_ref().expect("stable session"),
                session.instance_id
            ));
            self.store.save()?;
            if self.agents.close(session).await.is_err() {
                return self
                    .escalate(
                        session,
                        "Idle close rejected or ambiguous; inspect the pane",
                        key,
                        now,
                    )
                    .await;
            }
            self.store.save()?;
            self.decision(
                Some(session),
                "closed",
                "idle session closed; awaiting owner registry archive confirmation",
            )?;
        }
        Ok(())
    }
    async fn stall(&mut self, session: &SessionSummary, jobs: &[Job], now: u64) -> Result<()> {
        let key = session.session_key.as_ref().expect("stable session");
        let obs = &self.store.state.sessions[key];
        let elapsed = now.saturating_sub(obs.changed_at);
        let active = jobs
            .iter()
            .filter(|j| &j.session_key == key && j.active())
            .count();
        if active == 0 {
            if self
                .config
                .orphan_archive_seconds
                .is_some_and(|s| elapsed >= s)
                && session.status == "waiting"
                && !session.attached
                && session.windows == 1
            {
                let context = self.agents.context(session).await?;
                if !context.conversation_available || context.summary.is_empty() {
                    return Ok(());
                }
                self.decision(
                    Some(session),
                    "orphan",
                    "idle orphan summary captured; digest available in agent_summary",
                )?;
                self.archive(
                    session,
                    &format!("orphan:{key}:{}", session.content_hash),
                    now,
                )
                .await?;
            }
            return Ok(());
        }
        if elapsed < self.config.stall_seconds
            || obs.escalated
            || !matches!(session.status.as_str(), "working" | "waiting")
        {
            return Ok(());
        }
        if let Some(at) = obs.nudged_at {
            if now.saturating_sub(at) >= self.config.nudge_grace_seconds {
                self.escalate(
                    session,
                    "No output change after supervisor nudge; job needs attention",
                    &format!("stall:{key}:{}", obs.changed_at),
                    now,
                )
                .await?;
                self.store
                    .state
                    .sessions
                    .get_mut(key)
                    .expect("observed")
                    .escalated = true;
            }
        } else {
            let nudge_key = format!("nudge:{key}:{}", obs.changed_at);
            let text = "Please report progress on your assigned job. If blocked, report JOB BLOCKED with its job id and the blocker.";
            if self.reserve(Some(session), Action::Nudge, &nudge_key, now)? && self.live(session) {
                self.store
                    .state
                    .sessions
                    .get_mut(key)
                    .expect("observed")
                    .nudged_at = Some(now);
                self.store
                    .state
                    .sessions
                    .get_mut(key)
                    .expect("observed")
                    .nudge_echo_pending = true;
                self.store.save()?;
                self.agents.send(session, text).await?;
                self.decision(Some(session), "nudged", "one progress nudge delivered")?;
            }
        }
        Ok(())
    }
    async fn digest(&mut self, jobs: &[Job], now: u64) -> Result<()> {
        let day = now / 86400;
        if self.store.state.digest_day == Some(day)
            || now % 86400 / 3600 < u64::from(self.config.digest_hour_utc)
        {
            return Ok(());
        }
        let message = self.digest_text(jobs, now);
        if self.reserve(None, Action::Digest, &format!("digest:{day}"), now)? {
            self.platform
                .notify(&message, &format!("supervisor-digest-{day}"))
                .await?;
            self.store.state.digest_day = Some(day);
            self.store.save()?;
            self.decision(None, "digest", "daily digest delivered to Ryan")?;
        }
        Ok(())
    }
    #[must_use]
    pub fn digest_text(&self, jobs: &[Job], now: u64) -> String {
        let daily = self.store.state.daily.get(&(now / 86400));
        format!(
            "atmux daily digest (UTC day {})\nCompleted: {}; open: {}; blocked/escalations: {}; archived: {}\nQwen today: {}; actions today: {}; hour budgets Qwen {}/{} actions {}/{}\nMode: {}",
            now / 86400,
            daily.map_or(0, |d| d.completed),
            jobs.iter()
                .filter(|j| j.active() || j.state == "PENDING")
                .count(),
            jobs.iter()
                .filter(|j| matches!(j.state.as_str(), "ESCALATED" | "FAILED"))
                .count(),
            daily.map_or(0, |d| d.archived),
            daily.map_or(0, |d| d.model_calls),
            daily.map_or(0, |d| d.actions),
            self.store.state.model_calls,
            self.config.llm_calls_per_hour,
            self.store.state.actions,
            self.config.actions_per_hour,
            if self.config.dry_run {
                "dry run"
            } else {
                "active"
            }
        )
    }
}
pub(super) fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
pub(super) fn bounded(text: &str, bytes: usize) -> String {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].into()
}
