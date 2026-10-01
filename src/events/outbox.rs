//! Durable non-event publications. Unacknowledged records are never evicted.
use super::{
    sink::{Producer, validate_publication},
    spool::{atomic_json, private_directory, private_file},
};
use anyhow::{Context as _, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAX_RECORDS: usize = 4096;
const MAX_RECORD_BYTES: u64 = 192 * 1024;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    high: u64,
    acknowledged: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    seq: u64,
    topic: String,
    key: String,
    value: String,
}
impl Record {
    fn payload(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        let key = STANDARD.decode(&self.key)?;
        let value = STANDARD.decode(&self.value)?;
        validate_publication(&self.topic, &key, &value)?;
        Ok((key, value))
    }
}

#[derive(Debug, Default)]
struct State {
    manifest: Manifest,
    // Only small file indexes are held in memory, not publication bodies.
    pending: VecDeque<(u64, u64)>,
    bytes: u64,
}

#[derive(Debug)]
pub(crate) struct PublicationOutbox {
    directory: PathBuf,
    max_bytes: u64,
    state: Mutex<State>,
    drain: tokio::sync::Mutex<()>,
    writer_lock: File,
}
impl Drop for PublicationOutbox {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.writer_lock);
    }
}

impl PublicationOutbox {
    pub fn open(directory: PathBuf, max_bytes: u64) -> Result<Self> {
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
            .context("publication outbox is already owned")?;
        let manifest_path = directory.join("manifest.json");
        let mut state = State::default();
        if manifest_path.exists() {
            private_file(&manifest_path)?;
            if fs::metadata(&manifest_path)?.len() > 1024 {
                bail!("outbox manifest exceeds bounds");
            }
            state.manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
            if state.manifest.acknowledged > state.manifest.high {
                bail!("invalid outbox acknowledgement");
            }
        }
        for (count, entry) in fs::read_dir(&directory)?.enumerate() {
            if count > MAX_RECORDS + 3 {
                bail!("too many outbox files");
            }
            let path = entry?.path();
            let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
            private_file(&path)?;
            if matches!(name, "lock" | "manifest.json") {
                continue;
            }
            // Unrenamed writes were never acknowledged to an enqueue caller.
            if name == "manifest.tmp" || file_sequence(name, "tmp").is_some() {
                fs::remove_file(&path)?;
                continue;
            }
            let seq = file_sequence(name, "json").context("unexpected outbox file")?;
            if seq <= state.manifest.acknowledged {
                fs::remove_file(&path)?;
                continue;
            }
            let record = read_record(&path, seq)?;
            let bytes = fs::metadata(&path)?.len();
            state.manifest.high = state.manifest.high.max(record.seq);
            state.pending.push_back((seq, bytes));
            state.bytes += bytes;
            if state.pending.len() > MAX_RECORDS || state.bytes > max_bytes {
                bail!("pending outbox exceeds configured bounds; increase max_bytes to drain");
            }
        }
        state
            .pending
            .make_contiguous()
            .sort_unstable_by_key(|v| v.0);
        atomic_json(&manifest_path, &state.manifest)?;
        Ok(Self {
            directory,
            max_bytes,
            state: Mutex::new(state),
            drain: tokio::sync::Mutex::new(()),
            writer_lock: lock,
        })
    }

    /// Success means file and directory fsync completed. Capacity never evicts
    /// pending records; callers retain failed snapshots for a later enqueue.
    pub fn enqueue(&self, topic: &str, key: &[u8], value: &[u8]) -> Result<()> {
        validate_publication(topic, key, value)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = state
            .manifest
            .high
            .checked_add(1)
            .context("outbox sequence exhausted")?;
        let record = Record {
            seq,
            topic: topic.into(),
            key: STANDARD.encode(key),
            value: STANDARD.encode(value),
        };
        let bytes = serde_json::to_vec(&record)?.len() as u64;
        if state.pending.len() >= MAX_RECORDS || state.bytes + bytes > self.max_bytes {
            bail!("publication outbox is full");
        }
        atomic_json(&self.record_path(seq), &record)?;
        state.pending.push_back((seq, bytes));
        state.bytes += bytes;
        state.manifest.high = seq;
        atomic_json(&self.directory.join("manifest.json"), &state.manifest)
    }

    /// A single FIFO drain preserves per-key ordering, including retries.
    /// Acknowledgement follows broker success; ambiguous failures may replay.
    pub async fn publish_pending(&self, producer: &dyn Producer) -> Result<usize> {
        let _drain = self.drain.lock().await;
        let mut published = 0;
        for _ in 0..25 {
            let (head, acknowledged) = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.pending.front().copied(), state.manifest.acknowledged)
            };
            let Some((seq, bytes)) = head else {
                break;
            };
            if seq > acknowledged {
                let record = read_record(&self.record_path(seq), seq)?;
                let (key, value) = record.payload()?;
                producer.publish(&record.topic, &key, &value).await?;
                published += 1;
            }
            self.acknowledge(seq, bytes)?;
        }
        Ok(published)
    }

    fn acknowledge(&self, seq: u64, bytes: u64) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.pending.front().map(|v| v.0) != Some(seq) {
            bail!("outbox acknowledgement is out of order");
        }
        if seq > state.manifest.acknowledged {
            let manifest = Manifest {
                high: state.manifest.high,
                acknowledged: seq,
            };
            atomic_json(&self.directory.join("manifest.json"), &manifest)?;
            state.manifest = manifest;
        }
        // Durable acknowledgement prevents stale replay even if deletion is
        // interrupted. Keep the in-memory head until cleanup also succeeds.
        match fs::remove_file(self.record_path(seq)) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        File::open(&self.directory)?.sync_all()?;
        state.pending.pop_front();
        state.bytes -= bytes;
        Ok(())
    }

    fn record_path(&self, seq: u64) -> PathBuf {
        self.directory.join(format!("{seq:020}.json"))
    }
}

fn file_sequence(name: &str, extension: &str) -> Option<u64> {
    let (seq, ext) = name.rsplit_once('.')?;
    if ext != extension || seq.len() != 20 || !seq.bytes().all(|v| v.is_ascii_digit()) {
        return None;
    }
    seq.parse().ok().filter(|v| *v != 0)
}

fn read_record(path: &Path, seq: u64) -> Result<Record> {
    private_file(path)?;
    if fs::metadata(path)?.len() > MAX_RECORD_BYTES {
        bail!("outbox record exceeds bounds");
    }
    let record: Record = serde_json::from_slice(&fs::read(path)?)?;
    if record.seq != seq {
        bail!("outbox record sequence mismatch");
    }
    record.payload()?;
    Ok(record)
}
