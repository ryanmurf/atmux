use anyhow::{Context as _, Result, bail};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize, Serialize)]
pub(super) struct State {
    pub cursor: Option<String>,
    pub claims: BTreeMap<String, Claim>,
    pub sessions: BTreeMap<String, Observation>,
    pub finished: BTreeMap<String, Finished>,
    #[serde(default)]
    pub pending_archives: BTreeSet<String>,
    pub hour: u64,
    pub model_calls: u64,
    pub actions: u64,
    pub digest_day: Option<u64>,
    pub daily: BTreeMap<u64, Totals>,
}
#[derive(Default, Deserialize, Serialize)]
pub(super) struct Totals {
    pub completed: u64,
    pub blocked: u64,
    pub archived: u64,
    pub model_calls: u64,
    pub actions: u64,
}
#[derive(Deserialize, Serialize)]
pub(super) struct Claim {
    pub at: u64,
    pub session_key: Option<String>,
    pub instance: Option<String>,
}
#[derive(Default, Deserialize, Serialize)]
pub(super) struct Observation {
    pub instance: String,
    pub hash: String,
    pub status: String,
    pub changed_at: u64,
    pub nudged_at: Option<u64>,
    #[serde(default)]
    pub nudge_echo_pending: bool,
    pub escalated: bool,
    pub answered_at: Option<u64>,
    pub notified_at: Option<u64>,
}
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Finished {
    pub job: String,
    pub ledger: super::Job,
    pub completed: bool,
    pub project_updated: bool,
    pub hash: String,
    pub instance: String,
    pub quiet_since: u64,
}
pub(super) struct Store {
    directory: PathBuf,
    lock_file: File,
    pub state: State,
}
impl Drop for Store {
    fn drop(&mut self) {
        // A concurrent fork may briefly inherit the open file description
        // before exec closes it. Release ownership when this worker drops.
        let _ = fs2::FileExt::unlock(&self.lock_file);
    }
}
const MAX_BYTES: u64 = 4 * 1024 * 1024;
impl Store {
    pub fn open(directory: PathBuf) -> Result<Self> {
        if !directory.is_absolute() {
            bail!("supervisor store must be absolute");
        }
        for path in directory.ancestors() {
            if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                bail!("supervisor store cannot traverse symlinks");
            }
        }
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let lock = private_open(&directory.join("lock"), true)?;
        lock.try_lock_exclusive()
            .context("supervisor store already owned")?;
        let file = directory.join("state.json");
        let state = if file.exists() {
            let mut data = Vec::new();
            private_open(&file, false)?
                .take(MAX_BYTES + 1)
                .read_to_end(&mut data)?;
            if data.len() as u64 > MAX_BYTES {
                bail!("supervisor state exceeds bounds");
            }
            serde_json::from_slice(&data)?
        } else {
            State::default()
        };
        let store = Self {
            directory,
            lock_file: lock,
            state,
        };
        store.check()?;
        Ok(store)
    }
    fn check(&self) -> Result<()> {
        if self.state.claims.len() > 8192
            || self.state.sessions.len() > 512
            || self.state.finished.len() > 512
            || self.state.pending_archives.len() > 512
            || self.state.daily.len() > 32
        {
            bail!("supervisor state capacity reached; operator rotation required");
        }
        Ok(())
    }
    pub fn save(&self) -> Result<()> {
        self.check()?;
        let bytes = serde_json::to_vec(&self.state)?;
        if bytes.len() as u64 > MAX_BYTES {
            bail!("supervisor state exceeds bounds");
        }
        let tmp = self.directory.join("state.tmp");
        let mut file = private_open(&tmp, true)?;
        file.set_len(0)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(tmp, self.directory.join("state.json"))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
    pub fn budget(&mut self, now: u64) {
        // Daily digest claims can expire: digest_day independently prevents
        // same-day repeats. Pane claims expire only on a matching archive event.
        self.state
            .claims
            .retain(|_, c| c.session_key.is_some() || now.saturating_sub(c.at) < 32 * 86400);
        if self.state.hour != now / 3600 {
            self.state.hour = now / 3600;
            self.state.model_calls = 0;
            self.state.actions = 0;
        }
        self.state
            .daily
            .retain(|day, _| *day >= (now / 86400).saturating_sub(30));
    }
    pub fn totals(&mut self, now: u64) -> &mut Totals {
        self.state.daily.entry(now / 86400).or_default()
    }
}
fn private_open(path: &Path, create: bool) -> Result<File> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && (!meta.is_file()
            || meta.mode() & 0o077 != 0
            || meta.uid() != rustix::process::getuid().as_raw())
    {
        bail!("insecure supervisor state file");
    }
    Ok(OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)?)
}
