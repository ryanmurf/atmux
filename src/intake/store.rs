use super::{Candidate, Decision};
use anyhow::{Result, ensure};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkItem {
    pub key: String,
    pub channel: String,
    pub source: String,
    pub goal: String,
    pub criteria: String,
    pub metadata: Value,
    pub source_done: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkJob {
    pub item: WorkItem,
    pub message: crate::herodevs::ChannelMessage,
    pub assignment: Option<Candidate>,
    pub decision: Option<Decision>,
    pub reserved_name: Option<String>,
    pub dispatch_started: bool,
    pub dispatched: bool,
    pub blocked: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub cursors: BTreeMap<String, Option<String>>,
    pub triage_cache: BTreeMap<String, Vec<WorkItem>>,
    pub jobs: BTreeMap<String, WorkJob>,
    pub llm_hours: BTreeMap<u64, u64>,
    pub launch_hours: BTreeMap<u64, u64>,
    pub launch_days: BTreeMap<u64, u64>,
}
pub struct Store {
    pub state: State,
    directory: PathBuf,
    _lock: std::fs::File,
}
impl Store {
    pub fn open(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("intake.lock"))?;
        lock.try_lock_exclusive()
            .map_err(|_| anyhow::anyhow!("intake store already in use"))?;
        let state = read(directory)?;
        Ok(Self {
            state,
            directory: directory.into(),
            _lock: lock,
        })
    }
    pub fn save(&self) -> Result<()> {
        ensure!(
            self.state.jobs.len() <= 10000 && self.state.cursors.len() <= 10000,
            "intake store full"
        );
        let bytes = serde_json::to_vec(&self.state)?;
        ensure!(
            bytes.len() <= 16 * 1024 * 1024,
            "intake state exceeded bound"
        );
        let path = self.directory.join("state.pending");
        let mut file = std::fs::File::create(&path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(path, self.directory.join("state.json"))?;
        std::fs::File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
    pub fn reserve_llm(&mut self, now: u64, max: u64) -> Result<bool> {
        let hour = now / 3600;
        self.state
            .llm_hours
            .retain(|h, _| *h >= hour.saturating_sub(48));
        let count = self.state.llm_hours.entry(hour).or_default();
        if *count >= max {
            return Ok(false);
        }
        *count += 1;
        self.save()?;
        Ok(true)
    }
    pub fn reserve_launch(&mut self, now: u64, hourly: u64, daily: u64) -> Result<bool> {
        let hour = now / 3600;
        let day = now / 86400;
        self.state
            .launch_hours
            .retain(|h, _| *h >= hour.saturating_sub(48));
        self.state
            .launch_days
            .retain(|d, _| *d >= day.saturating_sub(7));
        if self.state.launch_hours.get(&hour).copied().unwrap_or(0) >= hourly
            || self.state.launch_days.get(&day).copied().unwrap_or(0) >= daily
        {
            return Ok(false);
        }
        *self.state.launch_hours.entry(hour).or_default() += 1;
        *self.state.launch_days.entry(day).or_default() += 1;
        self.save()?;
        Ok(true)
    }
}
pub fn read(directory: &Path) -> Result<State> {
    let path = directory.join("state.json");
    if !path.exists() {
        return Ok(State::default());
    }
    let mut bytes = vec![];
    std::fs::File::open(path)?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "intake store exceeded bound"
    );
    let state: State = serde_json::from_slice(&bytes)?;
    ensure!(
        state.jobs.len() <= 10000 && state.cursors.len() <= 10000,
        "intake store full"
    );
    Ok(state)
}
