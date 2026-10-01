use super::{AgentEvent, EventsConfig, MAX_EVENT_BYTES, MAX_PAGE_BYTES};
use anyhow::{Context as _, Result, bail};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufRead as _, BufReader, Write as _},
    os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredEvent {
    pub seq: u64,
    pub event: AgentEvent,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventPage {
    pub epoch: String,
    pub events: Vec<StoredEvent>,
    pub next: String,
    pub reset: bool,
}

#[derive(Clone, Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EventQuery {
    /// Opaque epoch:sequence cursor returned by the preceding page.
    pub after: Option<String>,
    /// Long poll seconds (0..=30).
    pub wait: Option<u64>,
    /// Maximum events (1..=100), also capped by 512 KiB per page.
    pub limit: Option<usize>,
    /// Exact event types, comma separated.
    pub types: Option<String>,
    pub machine: Option<String>,
    pub session_key: Option<String>,
    /// Exact needs-input reasons, comma separated.
    pub reasons: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct EventFilter {
    pub types: Vec<String>,
    pub machine: Option<String>,
    pub session_key: Option<String>,
    pub reasons: Vec<String>,
}
fn split(value: Option<&str>) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.len() > 1024 {
        bail!("event filter exceeds bounds");
    }
    let items: Vec<_> = value.split(',').map(str::to_owned).collect();
    if items.len() > 16
        || items.iter().any(|v| {
            v.is_empty()
                || v.chars()
                    .any(|c| !c.is_ascii_lowercase() && !matches!(c, '.' | '_'))
        })
    {
        bail!("invalid event filter");
    }
    Ok(items)
}

impl EventQuery {
    /// # Errors
    /// Rejects malformed cursors, filters and out-of-range bounds.
    pub fn validate(&self) -> Result<EventFilter> {
        if self.wait.unwrap_or(0) > 30 || !(1..=100).contains(&self.limit.unwrap_or(50)) {
            bail!("event wait must be 0..=30 and limit 1..=100");
        }
        if let Some(after) = &self.after {
            parse_cursor(after)?;
        }
        if let Some(machine) = &self.machine {
            crate::machine::validate_machine_id(machine)?;
        }
        if let Some(key) = &self.session_key
            && !crate::tmux::valid_session_key(key)
        {
            bail!("invalid session_key filter");
        }
        Ok(EventFilter {
            types: split(self.types.as_deref())?,
            machine: self.machine.clone(),
            session_key: self.session_key.clone(),
            reasons: split(self.reasons.as_deref())?,
        })
    }
}
impl EventFilter {
    fn matches(&self, event: &AgentEvent) -> bool {
        (self.types.is_empty() || self.types.contains(&event.event_type))
            && self.machine.as_ref().is_none_or(|v| v == &event.machine)
            && self
                .session_key
                .as_ref()
                .is_none_or(|v| v == &event.session_key)
            && (self.reasons.is_empty()
                || event
                    .reason
                    .as_ref()
                    .is_some_and(|v| self.reasons.contains(v)))
    }
}

fn parse_cursor(cursor: &str) -> Result<(&str, u64)> {
    let (epoch, seq) = cursor.split_once(':').context("invalid event cursor")?;
    if !crate::tmux::valid_session_key(epoch)
        || seq.len() > 20
        || seq.is_empty()
        || !seq.bytes().all(|v| v.is_ascii_digit())
    {
        bail!("invalid event cursor");
    }
    Ok((epoch, seq.parse()?))
}

#[derive(Debug, Deserialize, Serialize)]
struct Manifest {
    epoch: String,
    high: u64,
}
#[derive(Debug)]
struct Segment {
    path: PathBuf,
    bytes: u64,
    last: u64,
    modified_ms: u64,
}
#[derive(Debug)]
struct LogState {
    manifest: Manifest,
    records: VecDeque<StoredEvent>,
    ids: HashSet<String>,
    segments: VecDeque<Segment>,
}
#[derive(Debug)]
pub struct EventLog {
    directory: PathBuf,
    config: EventsConfig,
    state: Mutex<LogState>,
    changed: watch::Sender<u64>,
    writer_lock: File,
}

impl Drop for EventLog {
    fn drop(&mut self) {
        // An unrelated concurrent fork can briefly inherit this open file
        // description before CLOEXEC runs. Release the flock explicitly so
        // an immediate reopen need not wait for that child to exec.
        let _ = fs2::FileExt::unlock(&self.writer_lock);
    }
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        bail!("event directory must be user-owned mode 0700");
    }
    Ok(())
}

pub(crate) fn private_file(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o777 != 0o600
    {
        bail!("event file must be user-owned mode 0600");
    }
    Ok(())
}

pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if path.exists() {
        private_file(path)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .or_else(|error| {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error);
            }
            private_file(&tmp).map_err(std::io::Error::other)?;
            fs::remove_file(&tmp)?;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
        })?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    File::open(path.parent().context("event file has no parent")?)?.sync_all()?;
    Ok(())
}

/// Scan/native hooks and registry recovery can see the same end independently.
/// Use the already-bounded durable spool as the deduplication ledger, including
/// after restart, rather than an unbounded second cache.
fn same_lifecycle_transition(previous: &AgentEvent, current: &AgentEvent) -> bool {
    let attribute = match current.event_type.as_str() {
        "agent.exited" => "agent_pid",
        "session.closed" => "closed_ms",
        "session.archived" => "archived_ms",
        _ => return false,
    };
    previous.event_type == current.event_type
        && previous.machine == current.machine
        && previous.session_key == current.session_key
        && previous.instance_id == current.instance_id
        && current
            .detail
            .get(attribute)
            .is_some_and(|value| value.is_u64() && previous.detail.get(attribute) == Some(value))
}

impl EventLog {
    /// # Errors
    /// Rejects insecure directories, concurrent owners and damaged records.
    #[allow(clippy::too_many_lines)] // Validates every on-disk record before exposing the log.
    pub fn open(directory: PathBuf, config: EventsConfig) -> Result<Self> {
        private_directory(&directory)?;
        let lock_path = directory.join("lock");
        if lock_path.exists() {
            private_file(&lock_path)?;
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path)?;
        lock.try_lock_exclusive()
            .context("event log is already owned by another process")?;
        let manifest_path = directory.join("manifest.json");
        let mut paths: Vec<_> = fs::read_dir(&directory)?
            .filter_map(Result::ok)
            .map(|v| v.path())
            .filter(|v| v.extension().is_some_and(|v| v == "jsonl"))
            .collect();
        paths.sort();
        if paths.len() > 4096 {
            bail!("too many event segments");
        }
        let manifest = if manifest_path.exists() {
            private_file(&manifest_path)?;
            if fs::metadata(&manifest_path)?.len() > 4096 {
                bail!("event manifest exceeds bounds");
            }
            let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
            if !crate::tmux::valid_session_key(&manifest.epoch) {
                bail!("invalid spool epoch");
            }
            manifest
        } else {
            // A lost manifest invalidates old cursors; old segments cannot be
            // interpreted under the new epoch.
            for path in &paths {
                private_file(path)?;
                fs::remove_file(path)?;
            }
            paths.clear();
            Manifest {
                epoch: crate::tmux::new_session_key()?,
                high: 0,
            }
        };
        let mut state = LogState {
            manifest,
            records: VecDeque::new(),
            ids: HashSet::new(),
            segments: VecDeque::new(),
        };
        let mut total = 0;
        for path in paths {
            private_file(&path)?;
            let meta = fs::metadata(&path)?;
            total += meta.len();
            if meta.len() > config.segment_bytes + MAX_EVENT_BYTES as u64 + 4096
                || total > 2 * config.max_bytes
            {
                bail!("event spool exceeds configured read bounds");
            }
            let mut reader = BufReader::new(OpenOptions::new().read(true).write(true).open(&path)?);
            let mut last = 0;
            let mut offset = 0;
            loop {
                let mut line = Vec::new();
                // Segment size was bounded above; no unbounded line allocation.
                if reader.read_until(b'\n', &mut line)? == 0 {
                    break;
                }
                if line.last() != Some(&b'\n') {
                    // Crash during an append: discard only the incomplete tail.
                    reader.get_ref().set_len(offset)?;
                    reader.get_ref().sync_data()?;
                    break;
                }
                if line.len() > MAX_EVENT_BYTES + 4096 {
                    bail!("oversized event spool record");
                }
                let record: StoredEvent = serde_json::from_slice(&line)?;
                record.event.validate()?;
                if record.seq == 0 || state.records.back().is_some_and(|v| v.seq >= record.seq) {
                    bail!("unordered event spool");
                }
                state.manifest.high = state.manifest.high.max(record.seq);
                last = record.seq;
                state.ids.insert(record.event.id.clone());
                state.records.push_back(record);
                offset += line.len() as u64;
            }
            state.segments.push_back(Segment {
                path,
                bytes: offset,
                last,
                modified_ms: u64::try_from(
                    meta.modified()?
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_millis(),
                )
                .unwrap_or(u64::MAX),
            });
        }
        let (changed, _) = watch::channel(state.manifest.high);
        let log = Self {
            directory,
            config,
            state: Mutex::new(state),
            changed,
            writer_lock: lock,
        };
        {
            let mut state = log
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            log.retain(&mut state)?;
            atomic_json(&manifest_path, &state.manifest)?;
        }
        Ok(log)
    }

    /// # Errors
    /// Rejects invalid events or failed durable writes. Duplicate ids are no-ops.
    pub fn append(&self, event: AgentEvent) -> Result<()> {
        event.validate()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.ids.contains(&event.id)
            || matches!(
                event.event_type.as_str(),
                "agent.exited" | "session.closed" | "session.archived"
            ) && state
                .records
                .iter()
                .rev()
                .any(|stored| same_lifecycle_transition(&stored.event, &event))
        {
            return Ok(());
        }
        let seq = state
            .manifest
            .high
            .checked_add(1)
            .context("event sequence exhausted")?;
        let record = StoredEvent { seq, event };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        if state
            .segments
            .back()
            .is_none_or(|v| v.bytes + bytes.len() as u64 > self.config.segment_bytes)
        {
            state.segments.push_back(Segment {
                path: self.directory.join(format!("{seq:020}.jsonl")),
                bytes: 0,
                last: 0,
                modified_ms: crate::machine::now_ms(),
            });
        }
        let segment = state
            .segments
            .back_mut()
            .context("missing active event segment")?;
        if segment.path.exists() {
            private_file(&segment.path)?;
        }
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&segment.path)?;
        file.write_all(&bytes)?;
        file.sync_data()?;
        segment.bytes += bytes.len() as u64;
        segment.last = seq;
        segment.modified_ms = crate::machine::now_ms();
        state.ids.insert(record.event.id.clone());
        state.records.push_back(record);
        state.manifest.high = seq;
        atomic_json(&self.directory.join("manifest.json"), &state.manifest)?;
        self.retain(&mut state)?;
        self.changed.send_replace(seq);
        Ok(())
    }

    fn retain(&self, state: &mut LogState) -> Result<()> {
        let cutoff = crate::machine::now_ms().saturating_sub(self.config.retention_seconds * 1000);
        let mut total: u64 = state.segments.iter().map(|v| v.bytes).sum();
        while state
            .segments
            .front()
            .is_some_and(|v| total > self.config.max_bytes || v.modified_ms < cutoff)
        {
            let segment = state
                .segments
                .pop_front()
                .context("missing expired event segment")?;
            fs::remove_file(segment.path)?;
            total -= segment.bytes;
            while state.records.front().is_some_and(|v| v.seq <= segment.last) {
                let record = state
                    .records
                    .pop_front()
                    .context("missing expired event record")?;
                state.ids.remove(&record.event.id);
            }
        }
        Ok(())
    }

    /// # Errors
    /// Rejects malformed query parameters and failed retention writes.
    pub async fn read(&self, query: &EventQuery) -> Result<EventPage> {
        let filter = query.validate()?;
        let mut changed = self.changed.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(query.wait.unwrap_or(0));
        loop {
            let page = self.page(query, &filter)?;
            if !page.events.is_empty() || page.reset || query.wait.unwrap_or(0) == 0 {
                return Ok(page);
            }
            if tokio::time::timeout_at(deadline, changed.changed())
                .await
                .is_err()
            {
                return self.page(query, &filter);
            }
        }
    }

    fn page(&self, query: &EventQuery, filter: &EventFilter) -> Result<EventPage> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.retain(&mut state)?;
        let (epoch, mut after) = query
            .after
            .as_deref()
            .map(parse_cursor)
            .transpose()?
            .unwrap_or((&state.manifest.epoch, 0));
        let floor = state
            .records
            .front()
            .map_or(state.manifest.high, |v| v.seq - 1);
        let reset = query.after.is_some()
            && (epoch != state.manifest.epoch || after < floor || after > state.manifest.high);
        if reset {
            after = floor;
        }
        let mut next = after;
        let mut events = Vec::new();
        let mut size = 1024;
        for record in state.records.iter().filter(|v| v.seq > after) {
            if filter.matches(&record.event) {
                let length = serde_json::to_vec(record)?.len();
                if events.len() >= query.limit.unwrap_or(50) || size + length > MAX_PAGE_BYTES {
                    break;
                }
                size += length;
                events.push(record.clone());
            }
            next = record.seq;
        }
        Ok(EventPage {
            epoch: state.manifest.epoch.clone(),
            events,
            next: format!("{}:{next}", state.manifest.epoch),
            reset,
        })
    }
}
