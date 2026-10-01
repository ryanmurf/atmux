use super::{
    AgentEvent, EventLog, EventPage, EventQuery, EventsConfig, MAX_EVENT_BYTES, ProjectCache,
    hooks::{HookDelivery, map_hook, socket_path, validate_socket},
    outbox::PublicationOutbox,
    sink::{KafkaProducer, Producer, publish_page},
    spool::{atomic_json, private_directory, private_file},
};
use crate::{
    control::ControlPlane,
    remote::RemoteMachine,
    status::{AgentKind, AgentStatus},
    tmux::Session,
};
use anyhow::{Context as _, Result, bail};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
};

#[derive(Debug, Default)]
struct PaneState {
    generation: String,
    status: Option<AgentStatus>,
    emitted: HashSet<String>,
    hook_at: Option<std::time::Instant>,
    turn: Option<String>,
    needs_input: Option<String>,
    previous: Option<Session>,
    active: bool,
    started: bool,
}
#[derive(Debug, Default, Deserialize, Serialize)]
struct Checkpoints {
    sources: HashMap<String, String>,
    sink: Option<String>,
}
#[derive(Debug)]
pub struct EventService {
    pub owner: Arc<EventLog>,
    pub fleet: Arc<EventLog>,
    machine: String,
    coordinator: bool,
    outbox: Option<PublicationOutbox>,
    config: EventsConfig,
    panes: Mutex<HashMap<String, PaneState>>,
    projects: ProjectCache,
    checkpoints: Mutex<Checkpoints>,
    directory: PathBuf,
    socket: Mutex<Option<SocketGuard>>,
}
#[derive(Debug)]
struct SocketGuard {
    path: PathBuf,
    inode: u64,
    _lock: File,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|v| v.ino() == self.inode) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
impl EventService {
    /// # Errors
    /// Rejects an insecure, corrupt or already-owned spool.
    pub fn open(config: EventsConfig, machine: String, coordinator: bool) -> Result<Arc<Self>> {
        config.validate(coordinator)?;
        let directory = if let Some(path) = &config.directory {
            path.clone()
        } else {
            let dirs = directories::ProjectDirs::from("dev", "ryanmurf", "atmux")
                .context("atmux state directory unavailable")?;
            dirs.state_dir()
                .unwrap_or_else(|| dirs.data_local_dir())
                .join("events")
        };
        if !directory.is_absolute() {
            bail!("[events].directory must be absolute");
        }
        private_directory(&directory)?;
        let owner = Arc::new(EventLog::open(directory.join("owner"), config.clone())?);
        let fleet = if coordinator {
            Arc::new(EventLog::open(directory.join("fleet"), config.clone())?)
        } else {
            owner.clone()
        };
        let outbox = coordinator
            .then(|| PublicationOutbox::open(directory.join("outbox"), config.max_bytes))
            .transpose()?;
        let checkpoint_path = directory.join("checkpoints.json");
        let checkpoints = if checkpoint_path.exists() {
            private_file(&checkpoint_path)?;
            if fs::metadata(&checkpoint_path)?.len() > 64 * 1024 {
                bail!("event checkpoints exceed bounds");
            }
            serde_json::from_slice(&fs::read(checkpoint_path)?)?
        } else {
            Checkpoints::default()
        };
        Ok(Arc::new(Self {
            owner,
            fleet,
            machine,
            coordinator,
            outbox,
            config,
            panes: Mutex::new(HashMap::new()),
            projects: ProjectCache::default(),
            checkpoints: Mutex::new(checkpoints),
            directory,
            socket: Mutex::new(None),
        }))
    }

    /// Appends a validated owner event. A2/A3/A4 use this seam for their types.
    /// # Errors
    /// Rejects foreign machine ids or failed durable writes.
    pub fn emit(&self, event: AgentEvent) -> Result<()> {
        if event.machine != self.machine {
            bail!("cannot emit another owner's event");
        }
        self.owner.append(event)
    }

    /// Coordinator-originated metadata/lifecycle events about any machine.
    /// These never enter the owner feed or get re-exported through federation.
    /// # Errors
    /// Requires a coordinator, a valid envelope and a successful durable append.
    pub fn append_fleet(&self, event: AgentEvent) -> Result<()> {
        if !self.coordinator {
            bail!("fleet append requires a coordinator");
        }
        self.fleet.append(event)
    }

    /// Durably queues bytes for the configured sink without contacting Kafka.
    /// # Errors
    /// Rejects owner nodes, invalid/oversized records, full capacity or I/O errors.
    pub fn enqueue_publication(&self, topic: &str, key: &[u8], value: &[u8]) -> Result<()> {
        self.outbox
            .as_ref()
            .context("publication outbox requires a coordinator")?
            .enqueue(topic, key, value)
    }

    /// Latest generation-bound lifecycle attention, with status as a fallback.
    #[must_use]
    pub fn needs_input_reason(&self, session: &crate::control::SessionSummary) -> Option<String> {
        let fallback = || (session.status == "waiting").then(|| "idle_prompt".into());
        let Some(key) = &session.session_key else {
            return fallback();
        };
        let fleet = self
            .fleet
            .attention_event(&session.machine, key, &session.instance_id);
        let owner = (session.machine == self.machine)
            .then(|| {
                self.owner
                    .attention_event(&session.machine, key, &session.instance_id)
            })
            .flatten();
        let latest = [fleet, owner].into_iter().flatten().max_by(|a, b| {
            chrono::DateTime::parse_from_rfc3339(&a.time)
                .ok()
                .cmp(&chrono::DateTime::parse_from_rfc3339(&b.time).ok())
        });
        match latest.as_ref().map(|v| v.event_type.as_str()) {
            Some("agent.needs_input") => latest.and_then(|v| v.reason),
            Some("agent.turn_completed") => Some("idle_prompt".into()),
            Some("agent.working" | "agent.exited" | "session.closed" | "session.archived") => None,
            _ => fallback(),
        }
    }

    pub(crate) async fn publish_pending(&self, producer: &dyn Producer) -> Result<()> {
        let config = self
            .config
            .redpanda
            .as_ref()
            .context("Redpanda sink is disabled")?;
        let after = self
            .checkpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sink
            .clone();
        let events = match publish_page(&self.fleet, config, producer, after).await {
            Ok(next) => {
                let mut checkpoints = self
                    .checkpoints
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let previous = checkpoints.sink.replace(next);
                let saved = atomic_json(&self.directory.join("checkpoints.json"), &*checkpoints);
                if saved.is_err() {
                    checkpoints.sink = previous;
                }
                saved
            }
            Err(error) => Err(error),
        };
        // Attempt both streams even if one topic is offline. FIFO failures
        // stop the outbox before a newer snapshot can pass its predecessor.
        let publications = self
            .outbox
            .as_ref()
            .context("publication outbox unavailable")?
            .publish_pending(producer)
            .await;
        events.and(publications.map(|_| ()))
    }

    pub(crate) fn observe(&self, sessions: &[Session]) {
        let mut panes = self
            .panes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        panes.retain(|pane, state| {
            let present = sessions.iter().any(|v| &v.pane_id == pane);
            if !present
                && let Some(previous) = state.previous.clone()
                && previous.agent != AgentKind::Other
            {
                self.derive(&previous, state, "agent.exited", None);
            }
            present
        });
        for session in sessions {
            if session.session_key.is_none() {
                continue;
            }
            let state = panes.entry(session.pane_id.clone()).or_default();
            let generation = format!(
                "{}:{}:{:?}",
                session.pane_identity,
                session.agent_pid.unwrap_or(session.pane_pid),
                session.agent
            );
            if state.generation != generation {
                if let Some(previous) = state.previous.clone()
                    && previous.agent != AgentKind::Other
                {
                    self.derive(&previous, state, "agent.exited", None);
                }
                let reason = if state.generation.is_empty() {
                    "discovered"
                } else {
                    "relaunch"
                };
                state.generation = generation;
                state.emitted.clear();
                state.needs_input = None;
                state.hook_at = None;
                state.started = false;
                state.active = false;
                if session.agent != AgentKind::Other {
                    self.derive(session, state, "agent.started", Some(reason));
                }
            }
            // Native signals remain authoritative for this process generation.
            // Visible dialogs still supplement CLIs with incomplete hook coverage.
            let native = state.hook_at.is_some();
            if let Some(reason) = crate::status::input_reason(session.agent, &session.content) {
                self.derive(session, state, "agent.needs_input", Some(reason));
            } else if !native {
                if session.status == AgentStatus::Working
                    && state.status != Some(AgentStatus::Working)
                {
                    state.emitted.clear();
                    state.needs_input = None;
                    self.derive(session, state, "agent.working", None);
                } else if session.status == AgentStatus::Waiting
                    && state.status != Some(AgentStatus::Waiting)
                {
                    if state.status == Some(AgentStatus::Working) {
                        self.derive(session, state, "agent.turn_completed", None);
                    }
                    self.derive(session, state, "agent.needs_input", Some("idle_prompt"));
                }
            }
            state.status = Some(session.status);
            state.previous = Some(session.clone());
        }
    }

    fn derive(&self, session: &Session, state: &mut PaneState, kind: &str, reason: Option<&str>) {
        let key = format!("{kind}:{}", reason.unwrap_or_default());
        if state.emitted.contains(&key) {
            return;
        }
        if let Ok(mut event) = AgentEvent::from_session(&self.machine, session, kind, reason) {
            event.project = self.projects.get(&session.path);
            if self.emit(event).is_ok() {
                state.emitted.insert(key);
                if kind == "agent.started" {
                    state.started = true;
                }
                if kind == "agent.working" {
                    state.active = true;
                }
                if kind == "agent.turn_completed" {
                    state.active = false;
                }
                if kind == "agent.needs_input" {
                    state.needs_input = reason.map(str::to_owned);
                }
            }
        }
    }

    pub(crate) fn ingest_hook(&self, session: &Session, delivery: &HookDelivery) -> Result<()> {
        let mut event = AgentEvent::from_session(&self.machine, session, "agent.working", None)?;
        if !map_hook(&mut event, delivery) {
            return Ok(());
        }
        if event.event_type == "agent.exited" {
            event.detail["agent_pid"] = serde_json::json!(session.agent_pid);
        }
        event.project = self.projects.get(&session.path);
        let mut panes = self
            .panes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = panes.entry(session.pane_id.clone()).or_default();
        if state.generation.is_empty() {
            state.generation = format!(
                "{}:{}:{:?}",
                session.pane_identity,
                session.agent_pid.unwrap_or(session.pane_pid),
                session.agent
            );
            state.previous = Some(session.clone());
        }
        let turn = delivery
            .payload
            .get("turn_id")
            .and_then(serde_json::Value::as_str)
            .filter(|v| v.len() <= 128)
            .map(str::to_owned);
        if event.event_type == "agent.working"
            && (!state.active || turn.is_some() && turn != state.turn)
        {
            state.emitted.clear();
            state.needs_input = None;
        }
        if turn.is_some() {
            state.turn = turn;
        }
        // Discovery may precede the CLI SessionStart hook; one process start.
        let key = if event.event_type == "agent.started" {
            "agent.started:".to_owned()
        } else {
            format!(
                "{}:{}",
                event.event_type,
                event.reason.as_deref().unwrap_or_default()
            )
        };
        if event.event_type == "agent.started" && state.started || state.emitted.contains(&key) {
            return Ok(());
        }
        let kind = event.event_type.clone();
        let reason = event.reason.clone();
        self.emit(event)?;
        state.emitted.insert(key);
        if kind == "agent.started" {
            state.started = true;
        }
        if kind == "agent.turn_completed" || kind == "agent.needs_input" {
            state.active = false;
        }
        if kind == "agent.working" {
            state.active = true;
        }
        state.hook_at = Some(std::time::Instant::now());
        if kind == "agent.needs_input" {
            state.needs_input = reason;
        }
        if kind == "agent.working" {
            state.needs_input = None;
        }
        if kind == "agent.turn_completed" {
            self.derive(session, state, "agent.needs_input", Some("idle_prompt"));
        }
        Ok(())
    }

    /// # Errors
    /// Validates remote ownership and appends before checkpointing a batch.
    pub fn import_page(&self, machine: &str, page: &EventPage) -> Result<()> {
        EventQuery {
            after: Some(page.next.clone()),
            ..EventQuery::default()
        }
        .validate()?;
        if !page.next.starts_with(&format!("{}:", page.epoch)) || page.events.len() > 100 {
            bail!("invalid owner event page");
        }
        let mut last = 0;
        for record in &page.events {
            if record.event.machine != machine || record.seq <= last {
                bail!("invalid owner event origin or sequence");
            }
            record.event.validate()?;
            last = record.seq;
        }
        if page
            .next
            .split_once(':')
            .and_then(|(_, seq)| seq.parse::<u64>().ok())
            .is_none_or(|seq| seq < last)
        {
            bail!("owner cursor precedes its page");
        }
        for record in &page.events {
            self.fleet.append(record.event.clone())?;
        }
        let mut checkpoints = self
            .checkpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if checkpoints.sources.len() >= 256 && !checkpoints.sources.contains_key(machine) {
            bail!("too many event source checkpoints");
        }
        checkpoints
            .sources
            .insert(machine.to_owned(), page.next.clone());
        atomic_json(&self.directory.join("checkpoints.json"), &*checkpoints)
    }

    fn source_cursor(&self, machine: &str) -> Option<String> {
        self.checkpoints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sources
            .get(machine)
            .cloned()
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        control: &ControlPlane,
        coordinator: bool,
        owner: bool,
    ) -> Result<()> {
        if owner {
            self.start_listener(control)?;
            self.emit(AgentEvent::node_started(&self.machine)?)?;
        }
        if coordinator {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                loop {
                    let Some(service) = weak.upgrade() else {
                        break;
                    };
                    let page = service
                        .owner
                        .read(&EventQuery {
                            after: service.source_cursor(&service.machine),
                            wait: Some(1),
                            limit: Some(25),
                            ..EventQuery::default()
                        })
                        .await;
                    if let Ok(page) = page {
                        let _ = service.import_page(&service.machine, &page);
                    }
                    drop(service);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
        }
        if let Some(config) = self.config.redpanda.clone() {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                let mut producer = None;
                let mut delay = 1;
                loop {
                    let Some(service) = weak.upgrade() else {
                        break;
                    };
                    if producer.is_none() {
                        producer = KafkaProducer::connect(&config).await.ok();
                    }
                    let result = match &producer {
                        Some(producer) => service.publish_pending(producer).await,
                        None => Err(anyhow::anyhow!("producer unavailable")),
                    };
                    if result.is_ok() {
                        delay = 1;
                    } else {
                        producer = None;
                        delay = (delay * 2).min(60);
                    }
                    drop(service);
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
            });
        }
        Ok(())
    }

    pub(crate) fn federate(self: &Arc<Self>, machine: Arc<RemoteMachine>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut delay = 1;
            loop {
                let Some(service) = weak.upgrade() else {
                    break;
                };
                let after = service.source_cursor(&machine.id);
                let result = fetch_page(&machine, after.as_deref())
                    .await
                    .and_then(|page| service.import_page(&machine.id, &page));
                delay = if result.is_ok() {
                    1
                } else {
                    (delay * 2).min(60)
                };
                drop(service);
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        });
    }

    fn start_listener(self: &Arc<Self>, control: &ControlPlane) -> Result<()> {
        let path = socket_path()?;
        private_directory(path.parent().context("no hook directory")?)?;
        let lock_path = path.with_extension("lock");
        if lock_path.exists() {
            private_file(&lock_path)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(lock_path)?;
        lock.try_lock_exclusive()
            .context("another owner already has the hook socket")?;
        if path.exists() {
            validate_socket(&path)?;
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                bail!("hook socket is already listening");
            }
            fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        *self.socket.lock().unwrap() = Some(SocketGuard {
            inode: fs::symlink_metadata(&path)?.ino(),
            path,
            _lock: lock,
        });
        let weak = Arc::downgrade(self);
        let control = control.downgrade();
        let permits = Arc::new(Semaphore::new(8));
        tokio::spawn(async move {
            loop {
                let connection =
                    tokio::time::timeout(Duration::from_secs(1), listener.accept()).await;
                if weak.strong_count() == 0 {
                    break;
                }
                let Ok(Ok((stream, _))) = connection else {
                    continue;
                };
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    continue;
                };
                let service = weak.clone();
                let control = control.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = receive_hook(service, control, stream).await;
                });
            }
        });
        Ok(())
    }
}

pub(crate) async fn fetch_page(machine: &RemoteMachine, after: Option<&str>) -> Result<EventPage> {
    let mut path = "/api/v1/agent-events?wait=25&limit=25".to_owned();
    if let Some(after) = after {
        path.push_str("&after=");
        path.push_str(&crate::remote::encode_segment(after));
    }
    machine
        .get_json_with_timeout(&path, Duration::from_secs(30))
        .await
}

async fn receive_hook(
    service: Weak<EventService>,
    control: crate::control::WeakControlPlane,
    mut stream: UnixStream,
) -> Result<()> {
    let peer = stream.peer_cred()?;
    if peer.uid() != rustix::process::geteuid().as_raw() {
        bail!("hook from foreign user");
    }
    let mut bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_millis(180),
        (&mut stream)
            .take(MAX_EVENT_BYTES as u64 + 1)
            .read_to_end(&mut bytes),
    )
    .await??;
    if bytes.len() > MAX_EVENT_BYTES {
        bail!("oversized hook");
    }
    let delivery: HookDelivery = serde_json::from_slice(&bytes)?;
    let control = control.upgrade().context("owner stopped")?;
    let pane = delivery.pane.clone();
    let pane_pid =
        tokio::task::spawn_blocking(move || crate::tmux::Tmux::hook_pane_pid(&pane)).await??;
    if let Some(pid) = peer.pid().and_then(|v| u32::try_from(v).ok())
        && !descends_from(pid, pane_pid, delivery.parent_pid)
    {
        bail!("hook process does not descend from pane");
    }
    // The client stays alive for peer validation, but never waits on git/fsync.
    let _ = stream.write_all(b"1").await;
    let pane = delivery.pane.clone();
    let session = tokio::task::spawn_blocking(move || control.event_session(&pane)).await??;
    if session.pane_pid != pane_pid {
        bail!("hook pane process changed during validation");
    }
    let service = service.upgrade().context("event service stopped")?;
    tokio::task::spawn_blocking(move || service.ingest_hook(&session, &delivery)).await?
}

fn descends_from(mut pid: u32, pane: u32, claimed_parent: u32) -> bool {
    let started = std::time::Instant::now();
    for depth in 0..64 {
        if pid == pane {
            return true;
        }
        if pid <= 1 || started.elapsed() > Duration::from_millis(100) {
            return false;
        }
        let Some(parent) = parent_pid(pid) else {
            return false;
        };
        if depth == 0 && parent != claimed_parent {
            return false;
        }
        pid = parent;
    }
    false
}
#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    use std::io::Read as _;
    let mut text = String::new();
    File::open(format!("/proc/{pid}/stat"))
        .ok()?
        .take(4096)
        .read_to_string(&mut text)
        .ok()?;
    text.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}
#[cfg(not(target_os = "linux"))]
fn parent_pid(pid: u32) -> Option<u32> {
    let mut child = std::process::Command::new("/bin/ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().ok()?.is_some() {
            use std::io::Read as _;
            let mut output = String::new();
            child
                .stdout
                .take()?
                .take(128)
                .read_to_string(&mut output)
                .ok()?;
            return output.trim().parse().ok();
        }
        if start.elapsed() > Duration::from_millis(10) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
