//! Bounded native-log transport and durable desired-session recovery.
//!
//! A3 may adapt its archive record to this versioned manifest. Paths are owner
//! derived; imports never accept a target path or an executable from the wire.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use fs2::FileExt;
use rustix::fs::{AtFlags, Mode, OFlags};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::{
    config::{AgentProfile, Config, ProfileMode},
    old_sessions::{ResumeCandidate, ResumeHarness},
    tmux::{Session, Tmux},
};

pub const MAX_BUNDLE_BYTES: usize = 8 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 256 * 1024;
const MAX_FILES: usize = 128;
const MAX_RECORDS: usize = 512;
const MAX_MANIFEST_STRING: usize = 4096;
const SCHEMA: &str = "atmux.native-bundle/v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RegistryResumeConfig {
    pub enabled: bool,
    pub store_dir: Option<PathBuf>,
    pub max_bundle_bytes: usize,
    pub restore_on_start: bool,
    pub restore_machines: Vec<String>,
}

impl Default for RegistryResumeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            store_dir: None,
            max_bundle_bytes: MAX_BUNDLE_BYTES,
            restore_on_start: false,
            restore_machines: Vec::new(),
        }
    }
}

impl RegistryResumeConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(1024..=MAX_BUNDLE_BYTES).contains(&self.max_bundle_bytes) {
            bail!("[registry].max_bundle_bytes must be between 1024 and {MAX_BUNDLE_BYTES}");
        }
        if self.enabled
            && self
                .store_dir
                .as_ref()
                .is_none_or(|path| !path.is_absolute())
        {
            bail!("[registry].enabled requires an absolute store_dir");
        }
        if self.restore_on_start && (!self.enabled || self.restore_machines.is_empty()) {
            bail!(
                "[registry].restore_on_start requires enabled and a nonempty restore_machines allowlist"
            );
        }
        if self.restore_machines.len() > 64 {
            bail!("registry restore allowlist is too large");
        }
        for machine in &self.restore_machines {
            crate::machine::validate_machine_id(machine)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    pub schema: String,
    pub session_key: String,
    pub machine: String,
    pub name: String,
    pub harness: String,
    pub profile: String,
    pub mode: Option<ProfileMode>,
    pub native_id: String,
    pub project_root: PathBuf,
    pub cwd: PathBuf,
    pub remote: Option<String>,
    pub branch: Option<String>,
    /// Owner-derived relative path, retained only for Codex's date/filename.
    pub native_path: PathBuf,
    #[serde(default)]
    pub source_binding: Option<SourceBinding>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBinding {
    pub instance_id: String,
    pub agent_pid: u32,
    pub process_start: String,
    #[serde(default)]
    pub content_digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StopSourceRequest {
    pub session_key: String,
    pub native_id: String,
    pub binding: SourceBinding,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleFile {
    /// Relative to Claude's <native-id>/ sibling directory.
    pub path: PathBuf,
    pub data_base64: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBundle {
    pub manifest: BundleManifest,
    pub native_log: String,
    #[serde(default)]
    pub siblings: Vec<BundleFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionResumeRequest {
    pub session_key: String,
    pub machine: String,
    /// Leave the source running by default. Stop it only after target verification.
    #[serde(default, rename = "move")]
    pub move_source: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct ResumeResult {
    pub session_key: String,
    pub machine: String,
    pub name: String,
    pub pane_id: String,
    pub verified: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    pub session_keys: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Running,
    Closed,
    Archived,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct DesiredSession {
    pub session_key: String,
    pub machine: String,
    pub name: String,
    pub harness: String,
    pub state: DesiredState,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OwnerSnapshot {
    pub boot_id: String,
    pub server_id: Option<String>,
    pub running: Vec<String>,
    pub desired: Vec<DesiredSession>,
}

pub(crate) fn validate_snapshot(snapshot: &OwnerSnapshot) -> Result<()> {
    if snapshot.desired.len() > MAX_RECORDS
        || snapshot.running.len() > MAX_RECORDS
        || (!snapshot.boot_id.is_empty() && !crate::tmux::valid_session_key(&snapshot.boot_id))
        || snapshot
            .server_id
            .as_ref()
            .is_some_and(|id| id.len() > 256 || id.chars().any(char::is_control))
    {
        bail!("invalid/oversized owner registry snapshot");
    }
    for key in &snapshot.running {
        if !crate::tmux::valid_session_key(key) {
            bail!("invalid running session key");
        }
    }
    for entry in &snapshot.desired {
        crate::machine::validate_machine_id(&entry.machine)?;
        if !crate::tmux::valid_session_key(&entry.session_key)
            || entry.name.is_empty()
            || entry.name.len() > 100
            || entry.name.chars().any(char::is_control)
            || !matches!(entry.harness.as_str(), "claude" | "codex")
        {
            bail!("invalid desired session record");
        }
    }
    Ok(())
}

/// Selection requires a restart/reconnection signal and an explicit allowlist.
/// Owner tombstones always override a coordinator's older running record.
#[must_use]
pub fn select_restore(
    previous: &OwnerSnapshot,
    current: &OwnerSnapshot,
    reconnected: bool,
) -> Vec<String> {
    if !reconnected
        && previous.boot_id == current.boot_id
        && previous.server_id == current.server_id
    {
        return Vec::new();
    }
    let running: BTreeSet<_> = current.running.iter().collect();
    let closed: BTreeSet<_> = current
        .desired
        .iter()
        .filter(|entry| entry.state != DesiredState::Running)
        .map(|entry| &entry.session_key)
        .collect();
    previous
        .desired
        .iter()
        .chain(&current.desired)
        .filter(|entry| {
            entry.state == DesiredState::Running
                && !running.contains(&entry.session_key)
                && !closed.contains(&entry.session_key)
        })
        .map(|entry| entry.session_key.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_RECORDS)
        .collect()
}

fn valid_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.as_os_str().len() <= MAX_MANIFEST_STRING
        && !path.to_string_lossy().chars().any(char::is_control)
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

pub(crate) fn validate(bundle: &NativeBundle, limit: usize) -> Result<()> {
    let manifest = &bundle.manifest;
    if serde_json::to_vec(bundle)?.len() > limit || bundle.siblings.len() > MAX_FILES {
        bail!("native bundle exceeds configured size/file limit");
    }
    if manifest.schema != SCHEMA || !crate::tmux::valid_session_key(&manifest.session_key) {
        bail!("invalid native bundle schema/session key");
    }
    crate::machine::validate_machine_id(&manifest.machine)?;
    let harness = harness(&manifest.harness)?;
    if let Some(binding) = &manifest.source_binding
        && (!crate::tmux::valid_pane_identity(&binding.instance_id)
            || binding.agent_pid == 0
            || binding.process_start.is_empty()
            || binding.process_start.len() > 256
            || binding.process_start.chars().any(char::is_control)
            || binding.content_digest.len() != 64
            || !binding
                .content_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()))
    {
        bail!("invalid source process binding");
    }
    ResumeCandidate::imported(harness, &manifest.native_id)?;
    if manifest.name.is_empty()
        || manifest.name.len() > 80
        || !manifest
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("unsafe bundle session name");
    }
    if manifest.profile.is_empty() || manifest.profile.len() > 120 {
        bail!("invalid bundle profile");
    }
    for path in [&manifest.project_root, &manifest.cwd] {
        if !path.is_absolute()
            || path.as_os_str().len() > MAX_MANIFEST_STRING
            || path.to_string_lossy().chars().any(char::is_control)
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            bail!("invalid source project path");
        }
    }
    if !manifest.cwd.starts_with(&manifest.project_root) || !valid_relative(&manifest.native_path) {
        bail!("invalid source cwd/native path");
    }
    for value in [&manifest.remote, &manifest.branch].into_iter().flatten() {
        if value.is_empty()
            || value.starts_with('-')
            || value.len() > MAX_MANIFEST_STRING
            || value.chars().any(char::is_control)
        {
            bail!("invalid repository metadata");
        }
    }
    if let Some(mode) = &manifest.mode {
        let mut profile = target_profile_template(&manifest.profile, &manifest.harness);
        // A recorded synthetic mode has no selectable id; validate its controls
        // using the normal mode validator under a fixed valid id.
        profile.modes.push(ProfileMode {
            id: "imported".to_owned(),
            ..mode.clone()
        });
        let config = Config {
            profiles: vec![profile],
            ..Config::default()
        };
        config.validate_profiles()?;
    }
    let mut paths = BTreeSet::new();
    for file in &bundle.siblings {
        if !valid_relative(&file.path) || !paths.insert(&file.path) {
            bail!("unsafe or duplicate sibling path");
        }
        if STANDARD.decode(&file.data_base64)?.len() > limit {
            bail!("sibling exceeds size limit");
        }
    }
    if harness == ResumeHarness::Codex && !bundle.siblings.is_empty() {
        bail!("Codex bundle cannot contain Claude siblings");
    }
    Ok(())
}

fn target_profile_template(name: &str, harness: &str) -> AgentProfile {
    AgentProfile {
        name: name.to_owned(),
        harness: harness.to_owned(),
        command: harness.to_owned(),
        args: Vec::new(),
        env: BTreeMap::new(),
        claude_relaunch_permissions: None,
        modes: Vec::new(),
        inherit_discovered: false,
    }
}

fn harness(name: &str) -> Result<ResumeHarness> {
    match name {
        "claude" => Ok(ResumeHarness::Claude),
        "codex" => Ok(ResumeHarness::Codex),
        _ => bail!("native resume requires Claude or Codex"),
    }
}

fn encoded_project(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn mapped_path(value: &str, old: &Path, new: &Path) -> Option<String> {
    let relative = Path::new(value).strip_prefix(old).ok()?;
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
        && !relative.as_os_str().is_empty()
    {
        return None;
    }
    Some(
        if relative.as_os_str().is_empty() {
            new.to_path_buf()
        } else {
            new.join(relative)
        }
        .to_string_lossy()
        .into_owned(),
    )
}

/// Rewrites only verified native metadata fields, preserving tool/message text.
/// Each JSONL row is parsed independently and strictly bounded.
fn translate_log(log: &str, harness: ResumeHarness, old: &Path, new: &Path) -> Result<String> {
    let mut output = String::with_capacity(log.len());
    for line in log.split_inclusive('\n') {
        if line.len() > MAX_LINE_BYTES {
            bail!("native log row exceeds size limit");
        }
        let body = line.trim_end_matches('\n');
        if body.trim().is_empty() {
            output.push_str(line);
            continue;
        }
        let mut row: Value = serde_json::from_str(body).context("invalid native JSONL row")?;
        let cwd = match harness {
            ResumeHarness::Claude => row.get_mut("cwd"),
            ResumeHarness::Codex
                if row.get("type").and_then(Value::as_str) == Some("session_meta") =>
            {
                row.pointer_mut("/payload/cwd")
            }
            ResumeHarness::Codex => None,
        };
        let mut changed = false;
        if let Some(cwd) = cwd
            && let Some(value) = cwd.as_str()
            && let Some(mapped) = mapped_path(value, old, new)
        {
            *cwd = Value::String(mapped);
            changed = true;
        }
        if changed {
            output.push_str(&serde_json::to_string(&row)?);
            if line.ends_with('\n') {
                output.push('\n');
            }
        } else {
            output.push_str(line);
        }
        if output.len() > MAX_BUNDLE_BYTES {
            bail!("translated native log exceeds limit");
        }
    }
    Ok(output)
}

/// Opens every component with NOFOLLOW. Handles remain pinned across mutations.
fn directory(path: &Path, create: bool) -> Result<File> {
    if !path.is_absolute() {
        bail!("storage path must be absolute");
    }
    let mut held = File::from(rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    for component in path.components().skip(1) {
        let Component::Normal(name) = component else {
            bail!("unsafe storage path");
        };
        if create {
            match rustix::fs::mkdirat(&held, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
        }
        held = File::from(rustix::fs::openat(
            &held,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
    }
    Ok(held)
}

fn read_at(parent: &File, name: &std::ffi::OsStr, limit: usize) -> Result<Vec<u8>> {
    let mut file = File::from(rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?);
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.len() > limit as u64
    {
        bail!("unsafe or oversized native file");
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("native file exceeded size limit while reading");
    }
    Ok(bytes)
}

fn read_file(path: &Path, limit: usize) -> Result<Vec<u8>> {
    read_at(
        &directory(path.parent().context("file has no parent")?, false)?,
        path.file_name().context("file has no name")?,
        limit,
    )
}

fn write_file(path: &Path, bytes: &[u8], replace: bool) -> Result<()> {
    let parent = directory(path.parent().context("file has no parent")?, true)?;
    let name = path.file_name().context("file has no name")?;
    match rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => {
            let existing = read_at(&parent, name, MAX_BUNDLE_BYTES)?;
            if existing == bytes {
                return Ok(());
            }
            if !replace {
                bail!("conflicting existing native log; nothing overwritten");
            }
        }
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    let staging = format!(".import-{}", crate::tmux::new_session_key()?);
    let result = (|| {
        let mut file = File::from(rustix::fs::openat(
            &parent,
            &staging,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        file.write_all(bytes)?;
        file.sync_all()?;
        if replace {
            rustix::fs::renameat(&parent, &staging, &parent, name)?;
        } else {
            match rustix::fs::linkat(&parent, &staging, &parent, name, AtFlags::empty()) {
                Ok(()) => {}
                Err(rustix::io::Errno::EXIST)
                    if read_at(&parent, name, MAX_BUNDLE_BYTES)? == bytes => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })();
    let _ = rustix::fs::unlinkat(&parent, &staging, AtFlags::empty());
    parent.sync_all()?;
    result
}

#[derive(Debug)]
pub(crate) struct ResumeStore {
    root: PathBuf,
    pub(crate) boot_id: String,
    pub(crate) limit: usize,
}

impl ResumeStore {
    pub(crate) fn open(config: &RegistryResumeConfig) -> Result<Option<Self>> {
        config.validate()?;
        if !config.enabled {
            return Ok(None);
        }
        let root = crate::config::expand_tilde(
            config
                .store_dir
                .as_ref()
                .context("registry store is missing")?,
        );
        let held = directory(&root, true)?;
        let metadata = held.metadata()?;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
            bail!("registry store must be owner-only");
        }
        Ok(Some(Self {
            root,
            boot_id: crate::tmux::new_session_key()?,
            limit: config.max_bundle_bytes,
        }))
    }

    pub(crate) fn lock(&self) -> Result<File> {
        let parent = directory(&self.root, false)?;
        let file = File::from(rustix::fs::openat(
            &parent,
            "resume.lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            bail!("unsafe registry lock");
        }
        file.lock_exclusive()?;
        Ok(file)
    }

    pub(crate) fn save_bundle(&self, bundle: &NativeBundle) -> Result<()> {
        validate(bundle, self.limit)?;
        write_file(
            &self.bundle_path(&bundle.manifest.session_key)?,
            &serde_json::to_vec(bundle)?,
            true,
        )
    }
    pub(crate) fn load_bundle(&self, key: &str) -> Result<NativeBundle> {
        let bundle = serde_json::from_slice(&read_file(&self.bundle_path(key)?, self.limit)?)?;
        validate(&bundle, self.limit)?;
        Ok(bundle)
    }
    fn bundle_path(&self, key: &str) -> Result<PathBuf> {
        if !crate::tmux::valid_session_key(key) {
            bail!("invalid session key");
        }
        Ok(self.root.join("bundles").join(format!("{key}.json")))
    }
    pub(crate) fn snapshot(&self, machine: &str) -> Result<OwnerSnapshot> {
        crate::machine::validate_machine_id(machine)?;
        let path = self.root.join(format!("desired-{machine}.json"));
        match read_file(&path, 512 * 1024) {
            Ok(bytes) => {
                let state: OwnerSnapshot = serde_json::from_slice(&bytes)?;
                validate_snapshot(&state)?;
                Ok(state)
            }
            Err(error)
                if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) =>
            {
                Ok(OwnerSnapshot::default())
            }
            Err(error) => Err(error),
        }
    }
    pub(crate) fn save_snapshot(&self, machine: &str, snapshot: &OwnerSnapshot) -> Result<()> {
        crate::machine::validate_machine_id(machine)?;
        validate_snapshot(snapshot)?;
        let encoded = serde_json::to_vec(snapshot)?;
        if encoded.len() > 512 * 1024 {
            bail!("desired snapshot exceeds byte limit");
        }
        write_file(
            &self.root.join(format!("desired-{machine}.json")),
            &encoded,
            true,
        )
    }
    pub(crate) fn close(&self, machine: &str, key: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut snapshot = self.snapshot(machine)?;
        if let Some(entry) = snapshot
            .desired
            .iter_mut()
            .find(|entry| entry.session_key == key)
        {
            entry.state = DesiredState::Closed;
        }
        snapshot.running.retain(|running| running != key);
        self.save_snapshot(machine, &snapshot)
    }
}

fn git(project: &Path, args: &[&str]) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("repository lookup/checkout failed");
            }
            let mut result = String::new();
            child
                .stdout
                .take()
                .context("git stdout missing")?
                .take(MAX_MANIFEST_STRING as u64 + 1)
                .read_to_string(&mut result)?;
            if result.len() > MAX_MANIFEST_STRING {
                bail!("repository metadata exceeds limit");
            }
            return Ok(result.trim().to_owned());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("repository operation timed out");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn normalized_remote(remote: &str) -> Result<String> {
    // Reuse the credential-free parser by cloning only through launch_directory.
    // Matching accepts SSH/HTTPS equivalents without exposing embedded secrets.
    if remote.len() > MAX_MANIFEST_STRING || remote.chars().any(char::is_control) {
        bail!("unsafe repository remote");
    }
    let remote = remote.trim_end_matches('/').trim_end_matches(".git");
    let remote = if let Some(remote) = remote.strip_prefix("git@") {
        remote.replacen(':', "/", 1)
    } else if let Some(remote) = remote.strip_prefix("https://") {
        remote.to_owned()
    } else if let Some(remote) = remote.strip_prefix("ssh://git@") {
        remote.to_owned()
    } else {
        bail!("resume repository must use credential-free HTTPS or git SSH");
    };
    if remote.contains('@')
        || remote.contains('?')
        || remote.contains('#')
        || remote
            .split('/')
            .any(|part| part.is_empty() || part == ".." || part == ".")
    {
        bail!("unsafe repository remote");
    }
    Ok(remote)
}

fn resolve_project(config: &Config, manifest: &BundleManifest) -> Result<PathBuf> {
    let roots: Vec<_> = config
        .general
        .project_roots
        .iter()
        .map(|root| crate::config::expand_tilde(root))
        .filter_map(|root| root.canonicalize().ok())
        .collect();
    if let Some(remote) = &manifest.remote {
        let normalized = normalized_remote(remote)?;
        let mut queue: Vec<_> = roots.iter().map(|root| (root.clone(), 0)).collect();
        let mut entries = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut matches = BTreeSet::new();
        let mut found_remote = false;
        if let Some(branch) = &manifest.branch {
            git(
                roots.first().context("target has no project root")?,
                &["check-ref-format", "--branch", branch],
            )?;
        }
        while let Some((path, depth)) = queue.pop() {
            entries += 1;
            if entries > 4096 || Instant::now() > deadline {
                bail!("bounded repository search exhausted; configure a narrower project root");
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            if path.join(".git").exists() {
                if git(&path, &["remote", "get-url", "origin"])
                    .ok()
                    .and_then(|remote| normalized_remote(&remote).ok())
                    .as_deref()
                    == Some(&normalized)
                {
                    found_remote = true;
                    if manifest.branch.as_ref().is_none_or(|branch| {
                        git(&path, &["branch", "--show-current"]).ok().as_ref() == Some(branch)
                    }) {
                        matches.insert(path);
                    }
                }
                continue;
            }
            if depth < 4 {
                for entry in fs::read_dir(&path)?.take(4097 - entries) {
                    if entries + queue.len() > 4096 {
                        bail!("repository search entry budget exhausted");
                    }
                    let entry = entry?;
                    if !entry.file_name().to_string_lossy().starts_with('.')
                        && !matches!(
                            entry.file_name().to_str(),
                            Some("node_modules" | "target" | "vendor")
                        )
                    {
                        queue.push((entry.path(), depth + 1));
                    }
                }
            }
        }
        if matches.len() > 1 {
            bail!("repository remote is ambiguous on target");
        }
        if let Some(path) = matches.into_iter().next() {
            return Ok(path);
        }
        if found_remote {
            bail!("target repository is on a different branch; configure a matching worktree");
        }
        let parent = roots
            .first()
            .context("target has no project root for cloning")?;
        let created = crate::launch_directory::clone_repository(
            config,
            &parent.to_string_lossy(),
            remote,
            None,
        )?;
        if let Some(branch) = &manifest.branch {
            // The validated branch is a literal argv token, never an option. A
            // checkout applies only to a newly cloned project, never an existing one.
            git(&created, &["checkout", branch])?;
        }
        return Ok(created);
    }
    // No remote means local-only restoration; never guess a same-named folder
    // on a different machine.
    if manifest.machine != config.node.id {
        bail!("cross-machine resume requires a repository remote");
    }
    config
        .resolve_launch_directory(&manifest.project_root)
        .context("source project is unavailable")
}

#[derive(Debug)]
pub(crate) struct PreparedImport {
    pub(crate) directory: PathBuf,
    pub(crate) profile: AgentProfile,
    pub(crate) candidate: ResumeCandidate,
    pub(crate) local_bundle: NativeBundle,
}

#[allow(clippy::too_many_lines)] // Validate the entire bundle before publishing any native file.
pub(crate) fn import(
    config: &Config,
    bundle: &NativeBundle,
    limit: usize,
) -> Result<PreparedImport> {
    validate(bundle, limit)?;
    let manifest = &bundle.manifest;
    let matching: Vec<_> = config
        .profiles
        .iter()
        .filter(|profile| {
            profile.name == manifest.profile
                && profile.harness.eq_ignore_ascii_case(&manifest.harness)
        })
        .collect();
    let [profile] = matching.as_slice() else {
        bail!("target has no unique matching profile");
    };
    let profile = (*profile).clone();
    let harness = harness(&manifest.harness)?;
    let store = crate::old_sessions::profile_config_directory(&profile, harness)?;
    directory(&store, true)?;
    let project = resolve_project(config, manifest)?;
    let cwd = project.join(manifest.cwd.strip_prefix(&manifest.project_root)?);
    let cwd = config
        .resolve_launch_directory(&cwd)
        .context("translated cwd is missing or outside target project roots")?;
    directory(&cwd, false)?;
    let translated = translate_log(
        &bundle.native_log,
        harness,
        &manifest.project_root,
        &project,
    )?;
    // Prove the imported log belongs to the requested native id and cwd.
    let identity = translated
        .lines()
        .take(32)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|row| match harness {
            ResumeHarness::Claude => {
                row.get("sessionId").and_then(Value::as_str) == Some(&manifest.native_id)
                    && row.get("cwd").and_then(Value::as_str) == cwd.to_str()
            }
            ResumeHarness::Codex => {
                row.get("type").and_then(Value::as_str) == Some("session_meta")
                    && row.pointer("/payload/id").and_then(Value::as_str)
                        == Some(&manifest.native_id)
                    && row.pointer("/payload/cwd").and_then(Value::as_str) == cwd.to_str()
            }
        });
    if !identity {
        bail!("native bundle does not prove its conversation/cwd identity");
    }
    let path = match harness {
        ResumeHarness::Claude => store
            .join("projects")
            .join(encoded_project(&cwd))
            .join(format!("{}.jsonl", manifest.native_id)),
        ResumeHarness::Codex => {
            let parts: Vec<_> = manifest.native_path.components().collect();
            if parts.len() != 5
                || parts[0].as_os_str() != "sessions"
                || !parts[1..4].iter().enumerate().all(|(index, part)| {
                    let value = part.as_os_str().to_string_lossy();
                    value.len() == if index == 0 { 4 } else { 2 }
                        && value.bytes().all(|ch| ch.is_ascii_digit())
                })
                || !manifest.native_path.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .ends_with(&format!("{}.jsonl", manifest.native_id))
                })
            {
                bail!("invalid Codex rollout location");
            }
            store.join(&manifest.native_path)
        }
    };
    let mut local_bundle = bundle.clone();
    local_bundle.manifest.machine.clone_from(&config.node.id);
    local_bundle.manifest.project_root.clone_from(&project);
    local_bundle.manifest.cwd.clone_from(&cwd);
    path.strip_prefix(&store)?
        .clone_into(&mut local_bundle.manifest.native_path);
    local_bundle.manifest.source_binding = None;
    local_bundle.native_log.clone_from(&translated);
    let mut files = vec![(path.clone(), translated.into_bytes())];
    for file in &bundle.siblings {
        let mut bytes = STANDARD.decode(&file.data_base64)?;
        if file
            .path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            bytes = translate_log(
                std::str::from_utf8(&bytes)?,
                harness,
                &manifest.project_root,
                &project,
            )?
            .into_bytes();
        }
        files.push((path.with_extension("").join(&file.path), bytes));
    }
    for (local, (_, bytes)) in local_bundle.siblings.iter_mut().zip(files.iter().skip(1)) {
        local.data_base64 = STANDARD.encode(bytes);
    }
    validate(&local_bundle, limit)?;
    // Preflight all conflicts before publishing any native file.
    for (path, bytes) in &files {
        if fs::symlink_metadata(path).is_ok() && read_file(path, limit)? != *bytes {
            bail!("conflicting existing native log; nothing overwritten");
        }
        directory(path.parent().context("native file has no parent")?, true)?;
    }
    for (path, bytes) in files {
        write_file(&path, &bytes, false)?;
    }
    Ok(PreparedImport {
        directory: cwd,
        profile,
        candidate: ResumeCandidate::imported(harness, &manifest.native_id)?,
        local_bundle,
    })
}

pub(crate) fn export(config: &Config, session: &Session, limit: usize) -> Result<NativeBundle> {
    let project_root = git(&session.path, &["rev-parse", "--show-toplevel"])
        .map_or_else(|_| session.path.clone(), PathBuf::from);
    let remote = git(&project_root, &["remote", "get-url", "origin"]).ok();
    if let Some(remote) = &remote {
        normalized_remote(remote)?;
    }
    let branch = git(&project_root, &["branch", "--show-current"])
        .ok()
        .filter(|branch| !branch.is_empty());
    let profile = config
        .profiles
        .iter()
        .find(|profile| {
            profile.name == session.profile
                && profile
                    .harness
                    .eq_ignore_ascii_case(&session.agent.to_string())
        })
        .context("source profile has no matching configured store")?;
    let configured_store = crate::old_sessions::profile_config_directory(
        profile,
        harness(&profile.harness.to_ascii_lowercase())?,
    )?;
    let target = crate::transcript::native_resume_target_in_store(session, &configured_store)
        .context("native conversation is not yet available")?;
    if configured_store != target.config_dir {
        bail!("source profile store differs from native conversation store");
    }
    let log_path = target.log_path;
    let native_log = complete_log(read_file(&log_path, limit)?)?;
    let mode = Tmux::recorded_pane_mode(&session.pane_id)?;
    let harness_name = profile.harness.to_ascii_lowercase();
    let siblings = collect_siblings(&log_path, &harness_name, native_log.len(), limit)?;
    let mut bundle = NativeBundle {
        manifest: BundleManifest {
            schema: SCHEMA.to_owned(),
            session_key: session
                .session_key
                .clone()
                .context("pane has no stable session key")?,
            machine: config.node.id.clone(),
            name: session.name.clone(),
            harness: harness_name,
            profile: profile.name.clone(),
            mode,
            native_id: target.session_id,
            project_root,
            cwd: session.path.clone(),
            remote,
            branch,
            native_path: log_path.strip_prefix(&configured_store)?.to_owned(),
            source_binding: Some(SourceBinding {
                instance_id: session.pane_identity.clone(),
                agent_pid: session.agent_pid.context("source CLI is not running")?,
                process_start: crate::control::native_process_start_stamp(
                    session.agent_pid.context("source CLI is not running")?,
                )
                .context("source CLI generation unavailable")?,
                content_digest: String::new(),
            }),
        },
        native_log,
        siblings,
    };
    let content_digest = content_digest(&bundle);
    if let Some(binding) = &mut bundle.manifest.source_binding {
        binding.content_digest = content_digest;
    }
    validate(&bundle, limit)?;
    Ok(bundle)
}

pub(crate) fn content_digest(bundle: &NativeBundle) -> String {
    let mut digest = Sha256::new();
    digest.update((bundle.native_log.len() as u64).to_be_bytes());
    digest.update(bundle.native_log.as_bytes());
    let mut files: Vec<_> = bundle.siblings.iter().collect();
    files.sort_by_key(|file| &file.path);
    for file in files {
        let path = file.path.to_string_lossy();
        digest.update((path.len() as u64).to_be_bytes());
        digest.update(path.as_bytes());
        digest.update((file.data_base64.len() as u64).to_be_bytes());
        digest.update(file.data_base64.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn collect_siblings(
    log_path: &Path,
    harness: &str,
    main_bytes: usize,
    limit: usize,
) -> Result<Vec<BundleFile>> {
    let mut siblings = Vec::new();
    if harness == "claude" {
        let sibling_dir = log_path.with_extension("");
        if fs::symlink_metadata(&sibling_dir).is_ok() {
            directory(&sibling_dir, false)?;
            let mut stack = vec![sibling_dir.clone()];
            let mut bytes = main_bytes;
            let mut entries = 0;
            let deadline = Instant::now() + Duration::from_secs(2);
            while let Some(path) = stack.pop() {
                for entry in fs::read_dir(path)? {
                    entries += 1;
                    if entries > 4096 || Instant::now() >= deadline {
                        bail!("native sibling scan budget exceeded");
                    }
                    let entry = entry?;
                    let metadata = fs::symlink_metadata(entry.path())?;
                    if metadata.file_type().is_symlink() {
                        bail!("native sibling contains a symlink");
                    }
                    if metadata.is_dir() {
                        if entry
                            .path()
                            .strip_prefix(&sibling_dir)?
                            .components()
                            .count()
                            > 8
                        {
                            bail!("native sibling directory depth exceeded");
                        }
                        stack.push(entry.path());
                    } else {
                        if siblings.len() >= MAX_FILES {
                            bail!("native sibling file limit exceeded");
                        }
                        let data = read_file(&entry.path(), limit.saturating_sub(bytes))?;
                        bytes += data.len();
                        siblings.push(BundleFile {
                            path: entry.path().strip_prefix(&sibling_dir)?.to_owned(),
                            data_base64: STANDARD.encode(data),
                        });
                    }
                }
            }
        }
    }
    Ok(siblings)
}

fn complete_log(bytes: Vec<u8>) -> Result<String> {
    let mut log = String::from_utf8(bytes)?;
    // A live append may end mid-row. Retain every complete row and never import
    // a partial append; the next export picks it up when the writer finishes.
    if !log.ends_with('\n')
        && let Some(last) = log.rsplit('\n').next()
        && serde_json::from_str::<Value>(last).is_err()
    {
        log.truncate(log.rfind('\n').map_or(0, |position| position + 1));
    }
    Ok(log)
}

pub(crate) fn refresh_archive(
    config: &Config,
    mut bundle: NativeBundle,
    limit: usize,
) -> Result<NativeBundle> {
    validate(&bundle, limit)?;
    // Only the owner of this local bundle may read its configured native store.
    if bundle.manifest.machine != config.node.id {
        return Ok(bundle);
    }
    let profile = config
        .profiles
        .iter()
        .find(|profile| {
            profile.name == bundle.manifest.profile
                && profile
                    .harness
                    .eq_ignore_ascii_case(&bundle.manifest.harness)
        })
        .context("archive profile is unavailable")?;
    let root =
        crate::old_sessions::profile_config_directory(profile, harness(&bundle.manifest.harness)?)?;
    if bundle.manifest.harness == "claude"
        && bundle.manifest.native_path
            != PathBuf::from("projects")
                .join(encoded_project(&bundle.manifest.cwd))
                .join(format!("{}.jsonl", bundle.manifest.native_id))
    {
        bail!("archive native path differs from its conversation");
    }
    let path = root.join(&bundle.manifest.native_path);
    match read_file(&path, limit) {
        Ok(bytes) => {
            bundle.native_log = complete_log(bytes)?;
            bundle.siblings = collect_siblings(
                &path,
                &bundle.manifest.harness,
                bundle.native_log.len(),
                limit,
            )?;
        }
        Err(error)
            if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error),
    }
    bundle.manifest.source_binding = None;
    validate(&bundle, limit)?;
    Ok(bundle)
}

pub(crate) fn server_id() -> Option<String> {
    Tmux::output(["display-message", "-p", "#{pid}"])
        .ok()
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Persist close intent for every agent pane removed by a named tmux close.
/// Called before mutation by the web owner and TUI, including panes not yet
/// captured by the periodic observer.
pub(crate) fn record_named_close(
    store: &ResumeStore,
    config: &Config,
    sessions: &[Session],
    name: &str,
) -> Result<()> {
    let _lock = store.lock()?;
    let mut snapshot = store.snapshot(&config.node.id)?;
    for session in sessions.iter().filter(|session| session.name == name) {
        if !matches!(
            session.agent,
            crate::status::AgentKind::Claude | crate::status::AgentKind::Codex
        ) {
            continue;
        }
        let Some(key) = &session.session_key else {
            continue;
        };
        if let Ok(bundle) = export(config, session, store.limit) {
            store.save_bundle(&bundle)?;
        }
        if let Some(entry) = snapshot
            .desired
            .iter_mut()
            .find(|entry| &entry.session_key == key)
        {
            entry.state = DesiredState::Closed;
        } else {
            if snapshot.desired.len() >= MAX_RECORDS {
                bail!("desired session limit reached before close");
            }
            snapshot.desired.push(DesiredSession {
                session_key: key.clone(),
                machine: config.node.id.clone(),
                name: name.to_owned(),
                harness: session.agent.to_string().to_ascii_lowercase(),
                state: DesiredState::Closed,
            });
        }
        snapshot.running.retain(|running| running != key);
    }
    store.save_snapshot(&config.node.id, &snapshot)
}

pub(crate) fn observe_owner(
    store: &ResumeStore,
    config: &Config,
    sessions: &[Session],
) -> Result<OwnerSnapshot> {
    let _lock = store.lock()?;
    let mut snapshot = store.snapshot(&config.node.id)?;
    let server_id = server_id();
    let restarted = snapshot.boot_id != store.boot_id || snapshot.server_id != server_id;
    let previously_running: BTreeSet<_> = snapshot.running.iter().cloned().collect();
    let running: BTreeSet<_> = sessions
        .iter()
        .filter_map(|session| session.session_key.clone())
        .collect();
    for entry in &mut snapshot.desired {
        if entry.state == DesiredState::Running
            && !running.contains(&entry.session_key)
            && previously_running.contains(&entry.session_key)
            && !restarted
        {
            entry.state = DesiredState::Closed;
        }
    }
    for session in sessions {
        let Ok(bundle) = export(config, session, store.limit) else {
            continue;
        };
        store.save_bundle(&bundle)?;
        if let Some(entry) = snapshot
            .desired
            .iter_mut()
            .find(|entry| Some(&entry.session_key) == session.session_key.as_ref())
        {
            entry.name.clone_from(&session.name);
        } else {
            if snapshot.desired.len() >= MAX_RECORDS {
                bail!("desired session limit reached");
            }
            snapshot.desired.push(DesiredSession {
                session_key: bundle.manifest.session_key,
                machine: config.node.id.clone(),
                name: session.name.clone(),
                harness: bundle.manifest.harness,
                state: DesiredState::Running,
            });
        }
    }
    snapshot.boot_id.clone_from(&store.boot_id);
    snapshot.server_id = server_id;
    snapshot.running = running.into_iter().collect();
    store.save_snapshot(&config.node.id, &snapshot)?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture {
        root: PathBuf,
        config: Config,
        bundle: NativeBundle,
    }
    impl Fixture {
        fn new(harness: &str) -> Self {
            let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "atmux-resume-{}",
                crate::tmux::new_session_key().unwrap()
            ));
            let project = root.join("Users/ryan/IdeaProjects/atmux");
            let store = root.join("Users/ryan/.agent-hd");
            fs::create_dir_all(&project).unwrap();
            fs::create_dir_all(&store).unwrap();
            for args in [
                vec!["init", "-q", "-b", "main"],
                vec![
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/ryanmurf/atmux.git",
                ],
            ] {
                assert!(
                    Command::new("git")
                        .arg("-C")
                        .arg(&project)
                        .args(args)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            let mut config = Config::default();
            config.node.id = "mac".to_owned();
            config.general.project_roots = vec![root.join("Users/ryan/IdeaProjects")];
            config.general.favorite_dirs.clear();
            let mut profile = target_profile_template("hd", harness);
            profile.env.insert(
                if harness == "claude" {
                    "CLAUDE_CONFIG_DIR"
                } else {
                    "CODEX_HOME"
                }
                .to_owned(),
                store.to_string_lossy().into_owned(),
            );
            config.profiles = vec![profile];
            let id = "019a06d9-8341-7654-8abc-0123456789ab";
            let old = "/home/ryan/IdeaProjects/atmux";
            let log = if harness == "claude" {
                format!(
                    "{{\"sessionId\":\"{id}\",\"cwd\":\"{old}\",\"message\":{{\"content\":\"keep {old} as user text\"}}}}\n"
                )
            } else {
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{old}\"}}}}\n{{\"type\":\"message\",\"payload\":{{\"text\":\"{old}\"}}}}\n"
                )
            };
            let bundle = NativeBundle {
                manifest: BundleManifest {
                    schema: SCHEMA.to_owned(),
                    session_key: crate::tmux::new_session_key().unwrap(),
                    machine: "linux".to_owned(),
                    name: "agent".to_owned(),
                    harness: harness.to_owned(),
                    profile: "hd".to_owned(),
                    mode: None,
                    native_id: id.to_owned(),
                    project_root: PathBuf::from(old),
                    cwd: PathBuf::from(old),
                    remote: Some("git@github.com:ryanmurf/atmux.git".to_owned()),
                    branch: Some("main".to_owned()),
                    source_binding: None,
                    native_path: if harness == "claude" {
                        PathBuf::from(format!("projects/-home-ryan-IdeaProjects-atmux/{id}.jsonl"))
                    } else {
                        PathBuf::from(format!(
                            "sessions/2026/09/30/rollout-2026-09-30T12-00-00-{id}.jsonl"
                        ))
                    },
                },
                native_log: log,
                siblings: Vec::new(),
            };
            Self {
                root,
                config,
                bundle,
            }
        }
        fn store(&self) -> PathBuf {
            PathBuf::from(self.config.profiles[0].env.values().next().unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn claude_linux_to_macos_translates_metadata_and_siblings_only() -> Result<()> {
        let mut fixture = Fixture::new("claude");
        fixture.config.profiles[0].harness = "CLAUDE".to_owned();
        fixture.bundle.siblings.push(BundleFile {
            path: PathBuf::from("subagents/agent-1.jsonl"),
            data_base64: STANDARD.encode(&fixture.bundle.native_log),
        });
        fixture.bundle.siblings.push(BundleFile {
            path: PathBuf::from("tool-results/file.txt"),
            data_base64: STANDARD.encode("/home/ryan unchanged tool output"),
        });
        let prepared = import(&fixture.config, &fixture.bundle, MAX_BUNDLE_BYTES)?;
        assert!(
            prepared
                .directory
                .ends_with("Users/ryan/IdeaProjects/atmux")
        );
        assert_eq!(
            prepared.candidate.session_id(),
            fixture.bundle.manifest.native_id
        );
        let path = fixture
            .store()
            .join("projects")
            .join(encoded_project(&prepared.directory))
            .join(format!("{}.jsonl", fixture.bundle.manifest.native_id));
        let main = String::from_utf8(read_file(&path, MAX_BUNDLE_BYTES)?)?;
        let row: Value = serde_json::from_str(main.trim())?;
        assert_eq!(row["cwd"], prepared.directory.to_string_lossy().as_ref());
        assert_eq!(
            row["message"]["content"],
            "keep /home/ryan/IdeaProjects/atmux as user text"
        );
        assert_eq!(
            read_file(
                &path.with_extension("").join("subagents/agent-1.jsonl"),
                MAX_BUNDLE_BYTES
            )?,
            main.as_bytes()
        );
        assert_eq!(
            read_file(
                &path.with_extension("").join("tool-results/file.txt"),
                MAX_BUNDLE_BYTES
            )?,
            b"/home/ryan unchanged tool output"
        );
        import(&fixture.config, &fixture.bundle, MAX_BUNDLE_BYTES)?; // exact-content no-op
        Ok(())
    }

    #[test]
    fn codex_linux_to_macos_keeps_rollout_date_and_message_text() -> Result<()> {
        let fixture = Fixture::new("codex");
        let prepared = import(&fixture.config, &fixture.bundle, MAX_BUNDLE_BYTES)?;
        let log = String::from_utf8(read_file(
            &fixture.store().join(&fixture.bundle.manifest.native_path),
            MAX_BUNDLE_BYTES,
        )?)?;
        let rows: Vec<Value> = log
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        assert_eq!(
            rows[0]["payload"]["cwd"],
            prepared.directory.to_string_lossy().as_ref()
        );
        assert_eq!(rows[1]["payload"]["text"], "/home/ryan/IdeaProjects/atmux");
        Ok(())
    }

    #[test]
    fn import_refuses_profiles_conflicts_sizes_and_symlinks() -> Result<()> {
        let mut fixture = Fixture::new("claude");
        fixture.bundle.manifest.profile = "missing".to_owned();
        assert!(
            import(&fixture.config, &fixture.bundle, MAX_BUNDLE_BYTES)
                .unwrap_err()
                .to_string()
                .contains("matching profile")
        );
        fixture.bundle.manifest.profile = "hd".to_owned();
        assert!(import(&fixture.config, &fixture.bundle, 1024).is_ok());
        fixture.bundle.native_log = fixture.bundle.native_log.replace("keep", "different");
        assert!(
            import(&fixture.config, &fixture.bundle, MAX_BUNDLE_BYTES)
                .unwrap_err()
                .to_string()
                .contains("conflicting")
        );
        assert!(import(&fixture.config, &fixture.bundle, 100).is_err());
        let oversized = format!("{{\"cwd\":\"{}\"}}\n", "x".repeat(MAX_LINE_BYTES));
        assert!(
            translate_log(
                &oversized,
                ResumeHarness::Claude,
                Path::new("/home"),
                Path::new("/Users")
            )
            .is_err()
        );
        let symlink_path = fixture.store().join("linked");
        symlink(&fixture.root, &symlink_path)?;
        assert!(write_file(&symlink_path.join("native.jsonl"), b"x", false).is_err());
        let direct = fixture.store().join("log.jsonl");
        symlink(fixture.root.join("missing"), &direct)?;
        assert!(write_file(&direct, b"x", false).is_err());
        fixture.bundle.siblings.push(BundleFile {
            path: PathBuf::from("../escape"),
            data_base64: STANDARD.encode("x"),
        });
        assert!(import(&fixture.config, &fixture.bundle, MAX_BUNDLE_BYTES).is_err());
        fixture.bundle.siblings[0].path = PathBuf::from("tool-results/bad\nname");
        assert!(import(&fixture.config, &fixture.bundle, MAX_BUNDLE_BYTES).is_err());
        Ok(())
    }

    #[test]
    fn bounded_bundle_and_snapshot_store_survive_restart_and_tombstones() -> Result<()> {
        let fixture = Fixture::new("claude");
        let root = fixture.root.join("registry");
        fs::create_dir_all(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let config = RegistryResumeConfig {
            enabled: true,
            store_dir: Some(root),
            ..RegistryResumeConfig::default()
        };
        let first = ResumeStore::open(&config)?.unwrap();
        first.save_bundle(&fixture.bundle)?;
        let key = fixture.bundle.manifest.session_key.clone();
        first.save_snapshot(
            "linux",
            &OwnerSnapshot {
                boot_id: first.boot_id.clone(),
                server_id: Some("one".to_owned()),
                running: vec![key.clone()],
                desired: vec![DesiredSession {
                    session_key: key.clone(),
                    machine: "linux".to_owned(),
                    name: "agent".to_owned(),
                    harness: "claude".to_owned(),
                    state: DesiredState::Running,
                }],
            },
        )?;
        let second = ResumeStore::open(&config)?.unwrap();
        assert_ne!(first.boot_id, second.boot_id);
        assert_eq!(
            second.load_bundle(&key)?.native_log,
            fixture.bundle.native_log
        );
        second.close("linux", &key)?;
        assert_eq!(
            second.snapshot("linux")?.desired[0].state,
            DesiredState::Closed
        );
        assert!(second.snapshot("linux")?.running.is_empty());
        assert!(second.load_bundle(&key).is_ok()); // manual resume remains possible
        Ok(())
    }

    #[test]
    fn restore_requires_restart_and_never_resurrects_closed_or_archived_entries() {
        let mut previous = OwnerSnapshot {
            boot_id: "old".to_owned(),
            server_id: Some("server".to_owned()),
            ..OwnerSnapshot::default()
        };
        for (key, state) in [
            ("lost", DesiredState::Running),
            ("live", DesiredState::Running),
            ("closed", DesiredState::Closed),
            ("archived", DesiredState::Archived),
            ("closed-later", DesiredState::Running),
        ] {
            previous.desired.push(DesiredSession {
                session_key: key.to_owned(),
                machine: "mac".to_owned(),
                name: key.to_owned(),
                harness: "claude".to_owned(),
                state,
            });
        }
        let mut current = previous.clone();
        current.running = vec!["live".to_owned()];
        current
            .desired
            .iter_mut()
            .find(|entry| entry.session_key == "closed-later")
            .unwrap()
            .state = DesiredState::Closed;
        assert!(select_restore(&previous, &current, false).is_empty());
        current.boot_id = "new".to_owned();
        assert_eq!(select_restore(&previous, &current, false), ["lost"]);
        current.boot_id = "old".to_owned();
        assert_eq!(select_restore(&previous, &current, true), ["lost"]);
        current.server_id = Some("different-server".to_owned());
        assert_eq!(select_restore(&previous, &current, false), ["lost"]);
    }

    #[test]
    fn registry_is_opt_in_and_restore_requires_explicit_machine_allowlist() {
        assert!(
            ResumeStore::open(&RegistryResumeConfig::default())
                .unwrap()
                .is_none()
        );
        let config = RegistryResumeConfig {
            restore_on_start: true,
            ..RegistryResumeConfig::default()
        };
        assert!(config.validate().is_err());
        for remote in [
            "https://user:secret@example.com/repo",
            "https://example.com/repo?token=x",
            "/local/repo",
            "--upload-pack=evil",
        ] {
            assert!(normalized_remote(remote).is_err());
        }
        assert_eq!(
            normalized_remote("git@github.com:ryanmurf/atmux.git").unwrap(),
            normalized_remote("https://github.com/ryanmurf/atmux").unwrap()
        );
    }

    #[test]
    fn archive_refresh_keeps_the_final_complete_turn_after_a_close() -> Result<()> {
        let fixture = Fixture::new("claude");
        let prepared = import(&fixture.config, &fixture.bundle, MAX_BUNDLE_BYTES)?;
        let path = fixture
            .store()
            .join(&prepared.local_bundle.manifest.native_path);
        let mut file = fs::OpenOptions::new().append(true).open(&path)?;
        writeln!(
            file,
            "{{\"type\":\"assistant\",\"message\":{{\"content\":\"final turn\"}}}}"
        )?;
        write!(file, "{{\"type\":")?;
        drop(file);
        let refreshed = refresh_archive(&fixture.config, prepared.local_bundle, MAX_BUNDLE_BYTES)?;
        assert!(refreshed.native_log.contains("final turn"));
        assert!(!refreshed.native_log.ends_with("{\"type\":"));
        assert_eq!(refreshed.manifest.machine, "mac");
        fs::remove_file(path)?;
        let cached = refresh_archive(&fixture.config, refreshed.clone(), MAX_BUNDLE_BYTES)?;
        assert_eq!(cached.native_log, refreshed.native_log);
        Ok(())
    }

    #[test]
    fn repository_lookup_requires_a_matching_branch_and_unambiguous_remote() -> Result<()> {
        let fixture = Fixture::new("claude");
        let project = fixture.config.general.project_roots[0].join("atmux");
        let mut manifest = fixture.bundle.manifest.clone();
        manifest.branch = Some("different".to_owned());
        assert!(
            resolve_project(&fixture.config, &manifest)
                .unwrap_err()
                .to_string()
                .contains("different branch")
        );
        manifest.branch = Some("main".to_owned());
        assert_eq!(resolve_project(&fixture.config, &manifest)?, project);
        manifest.branch = Some("--orphan=evil".to_owned());
        assert!(
            validate(
                &NativeBundle {
                    manifest,
                    ..fixture.bundle.clone()
                },
                MAX_BUNDLE_BYTES
            )
            .is_err()
        );
        Ok(())
    }
}
