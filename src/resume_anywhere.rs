//! Native resume using A3's checksummed archive and authoritative registry.
//! Compressed transfer, extraction and translation stream through registry-owned
//! staging descriptors. Browser/MCP callers never supply native ids or paths.
use crate::{
    config::{AgentProfile, Config, ProfileMode},
    old_sessions::{ResumeCandidate, ResumeHarness},
    registry::{BundleManifest, Registry, StoredRecord},
    tmux::Tmux,
};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{AtFlags, Mode, OFlags};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const MAX_LINE_BYTES: usize = 256 * 1024;
const MAX_FILES: usize = 4096;
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_MANIFEST_STRING: usize = 4096;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionResumeRequest {
    pub session_key: String,
    pub machine: String,
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
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBinding {
    pub instance_id: String,
    pub agent_pid: u32,
    pub process_start: String,
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
pub(crate) struct ImportResponse {
    pub result: ResumeResult,
    pub record: StoredRecord,
}

/// Temporary native files belong to the registry staging area, not a second store.
pub(crate) struct StagedFile {
    registry: Arc<Registry>,
    name: String,
    pub(crate) file: File,
}
impl StagedFile {
    pub(crate) fn new(registry: &Arc<Registry>) -> Result<Self> {
        let (name, file) = registry.incoming_file()?;
        Ok(Self {
            registry: registry.clone(),
            name,
            file,
        })
    }
}
impl Drop for StagedFile {
    fn drop(&mut self) {
        self.registry.discard_incoming(&self.name);
    }
}
pub(crate) struct ArchiveEntry {
    path: String,
    data: Option<StagedFile>,
}
pub(crate) struct DecodedArchive {
    pub(crate) manifest: BundleManifest,
    entries: Vec<ArchiveEntry>,
}
struct LimitedRead<R> {
    inner: R,
    remaining: u64,
}
impl<R: Read> Read for LimitedRead<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::other("archive expansion exceeds cap"));
        }
        let count = bytes
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let got = self.inner.read(&mut bytes[..count])?;
        self.remaining -= got as u64;
        Ok(got)
    }
}
fn valid_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.as_os_str().len() <= 8192
        && !path.to_string_lossy().chars().any(char::is_control)
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

#[allow(clippy::too_many_lines)] // Validate every archive entry before publishing native files.
pub(crate) fn decode(registry: &Arc<Registry>, mut file: File) -> Result<DecodedArchive> {
    ensure!(
        file.metadata()?.len() <= registry.bundle_limit(),
        "compressed bundle exceeds cap"
    );
    file.seek(SeekFrom::Start(0))?;
    let limit = registry
        .bundle_limit()
        .checked_add((MAX_FILES * 2048 + MAX_MANIFEST_BYTES) as u64)
        .context("archive limit overflow")?;
    let reader = LimitedRead {
        inner: flate2::read::GzDecoder::new(file),
        remaining: limit,
    };
    let mut archive = tar::Archive::new(reader);
    let mut entries = archive.entries()?;
    let mut first = entries.next().context("archive has no manifest")??;
    ensure!(
        first.path()?.as_ref() == Path::new("manifest.json")
            && first.header().entry_type().is_file()
            && first.size() <= MAX_MANIFEST_BYTES as u64,
        "invalid archive manifest"
    );
    let mut bytes = Vec::new();
    first.read_to_end(&mut bytes)?;
    let mut manifest: BundleManifest = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest.schema == "atmux.session.archive/v1" && manifest.files.len() <= MAX_FILES,
        "invalid archive schema/file count"
    );
    manifest.session.validate()?;
    let spec = ResumeSpec::from_record(&manifest.session)?;
    ensure!(valid_relative(&spec.native_path), "unsafe native location");
    let mut expected = BTreeMap::new();
    let mut total = bytes.len() as u64;
    for item in &manifest.files {
        ensure!(
            valid_relative(Path::new(&item.path)) && item.path.starts_with("native/"),
            "unsafe archive path"
        );
        ensure!(
            item.sha256.len() == 64
                && item
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "invalid file checksum"
        );
        ensure!(!item.directory || item.bytes == 0, "directory has payload");
        total = total
            .checked_add(item.bytes)
            .context("archive size overflow")?;
        ensure!(
            total <= registry.bundle_limit(),
            "uncompressed bundle exceeds cap"
        );
        ensure!(
            expected.insert(item.path.clone(), item).is_none(),
            "duplicate manifest path"
        );
    }
    drop(first);
    let mut seen = BTreeSet::new();
    let mut native_entries = Vec::new();
    for entry in entries {
        let mut entry = entry?;
        let path = entry
            .path()?
            .to_str()
            .context("non-UTF8 archive path")?
            .to_owned();
        let item = expected.get(&path).context("unlisted archive entry")?;
        ensure!(seen.insert(path.clone()), "duplicate archive entry");
        ensure!(
            entry.size() == item.bytes
                && (if item.directory {
                    entry.header().entry_type().is_dir()
                } else {
                    entry.header().entry_type().is_file()
                }),
            "archive entry type/size mismatch"
        );
        let mut digest = Sha256::new();
        let data = if item.directory {
            None
        } else {
            let mut staged = StagedFile::new(registry)?;
            let mut buffer = vec![0_u8; 64 * 1024];
            loop {
                let count = entry.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
                staged.file.write_all(&buffer[..count])?;
            }
            staged.file.sync_all()?;
            staged.file.seek(SeekFrom::Start(0))?;
            Some(staged)
        };
        ensure!(
            format!("{:x}", digest.finalize()) == item.sha256,
            "archive checksum mismatch"
        );
        native_entries.push(ArchiveEntry { path, data });
    }
    ensure!(seen.len() == expected.len(), "archive entries are missing");
    let mut trailing = archive.into_inner();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = trailing.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        ensure!(
            buffer[..count].iter().all(|byte| *byte == 0),
            "trailing archive payload"
        );
    }
    let native = format!("native/{}", spec.native_path.to_string_lossy());
    ensure!(seen.contains(&native), "archive has no native conversation");
    Ok(DecodedArchive {
        manifest,
        entries: native_entries,
    })
}

pub(crate) fn content_digest(manifest: &BundleManifest) -> String {
    let mut digest = Sha256::new();
    let mut files: Vec<_> = manifest.files.iter().collect();
    files.sort_by_key(|file| &file.path);
    for file in files {
        digest.update((file.path.len() as u64).to_be_bytes());
        digest.update(file.path.as_bytes());
        digest.update(file.bytes.to_be_bytes());
        digest.update(file.sha256.as_bytes());
        digest.update([u8::from(file.directory)]);
    }
    format!("{:x}", digest.finalize())
}
pub(crate) fn source_proof(manifest: &BundleManifest) -> Result<StopSourceRequest> {
    let stored = &manifest.session;
    Ok(StopSourceRequest {
        session_key: stored.record.session_key.clone(),
        native_id: stored
            .native
            .as_ref()
            .context("source has no native identity")?
            .session_id
            .clone(),
        binding: SourceBinding {
            instance_id: stored.instance_id.clone(),
            agent_pid: stored.agent_pid.context("source CLI exited")?,
            process_start: stored
                .process_start
                .clone()
                .context("source generation unavailable")?,
            content_digest: content_digest(manifest),
        },
    })
}

/// Derived translation inputs, not an archive manifest or persisted record.
struct ResumeSpec {
    machine: String,
    profile: String,
    harness: String,
    native_id: String,
    project_root: PathBuf,
    cwd: PathBuf,
    remote: Option<String>,
    branch: Option<String>,
    native_path: PathBuf,
}
impl ResumeSpec {
    fn from_record(stored: &StoredRecord) -> Result<Self> {
        let record = &stored.record;
        let native = stored
            .native
            .as_ref()
            .context("archive has no native log")?;
        let harness = harness(&record.harness)?;
        ResumeCandidate::imported(harness, &native.session_id)?;
        let project_root = PathBuf::from(&record.project.root);
        let cwd = PathBuf::from(&record.cwd);
        for path in [&project_root, &cwd] {
            ensure!(
                path.is_absolute()
                    && path.as_os_str().len() <= MAX_MANIFEST_STRING
                    && !path.to_string_lossy().chars().any(char::is_control)
                    && !path
                        .components()
                        .any(|part| matches!(part, Component::ParentDir | Component::CurDir)),
                "invalid source project path"
            );
        }
        ensure!(cwd.starts_with(&project_root), "cwd outside source project");
        Ok(Self {
            machine: record.machine.clone(),
            profile: record.profile.clone(),
            harness: record.harness.clone(),
            native_id: native.session_id.clone(),
            project_root,
            cwd,
            remote: record.project.remote.clone(),
            branch: record.project.branch.clone(),
            native_path: native
                .log_path
                .strip_prefix(&native.config_root)?
                .to_owned(),
        })
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
fn translate_row(row: &mut Value, provider: ResumeHarness, old: &Path, new: &Path) -> bool {
    let cwd = match provider {
        ResumeHarness::Claude => row.get_mut("cwd"),
        ResumeHarness::Codex if row.get("type").and_then(Value::as_str) == Some("session_meta") => {
            row.pointer_mut("/payload/cwd")
        }
        ResumeHarness::Codex => None,
    };
    if let Some(cwd) = cwd
        && let Some(value) = cwd.as_str()
        && let Some(mapped) = mapped_path(value, old, new)
    {
        *cwd = Value::String(mapped);
        true
    } else {
        false
    }
}
fn identity(row: &Value, provider: ResumeHarness, native: &str, cwd: &Path) -> bool {
    match provider {
        ResumeHarness::Claude => {
            row.get("sessionId").and_then(Value::as_str) == Some(native)
                && row.get("cwd").and_then(Value::as_str) == cwd.to_str()
        }
        ResumeHarness::Codex => {
            row.get("type").and_then(Value::as_str) == Some("session_meta")
                && row.pointer("/payload/id").and_then(Value::as_str) == Some(native)
                && row.pointer("/payload/cwd").and_then(Value::as_str) == cwd.to_str()
        }
    }
}
fn translate_file(
    registry: &Arc<Registry>,
    file: &mut File,
    spec: &ResumeSpec,
    project: &Path,
    cwd: &Path,
    main: bool,
) -> Result<StagedFile> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file);
    let mut output = StagedFile::new(registry)?;
    let provider = harness(&spec.harness)?;
    let mut proved = false;
    let mut total = 0_u64;
    loop {
        let mut line = Vec::new();
        let got = reader
            .by_ref()
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if got == 0 {
            break;
        }
        ensure!(line.len() <= MAX_LINE_BYTES, "native JSONL row exceeds cap");
        let body = std::str::from_utf8(&line)?.trim_end_matches('\n');
        if !body.trim().is_empty() {
            let mut row: Value = match serde_json::from_str(body) {
                Ok(row) => row,
                Err(_) if !line.ends_with(b"\n") => break,
                Err(error) => return Err(error.into()),
            };
            let changed = translate_row(&mut row, provider, &spec.project_root, project);
            proved |= identity(&row, provider, &spec.native_id, cwd);
            if changed {
                let mut mapped = serde_json::to_vec(&row)?;
                if line.ends_with(b"\n") {
                    mapped.push(b'\n');
                }
                line = mapped;
            }
        }
        total = total
            .checked_add(line.len() as u64)
            .context("native size overflow")?;
        ensure!(
            total <= registry.bundle_limit(),
            "translated log exceeds cap"
        );
        output.file.write_all(&line)?;
    }
    ensure!(
        !main || proved,
        "native log does not prove conversation/cwd identity"
    );
    output.file.sync_all()?;
    output.file.seek(SeekFrom::Start(0))?;
    Ok(output)
}

pub(crate) struct PreparedImport {
    pub(crate) directory: PathBuf,
    pub(crate) profile: AgentProfile,
    pub(crate) candidate: ResumeCandidate,
    pub(crate) mode: Option<ProfileMode>,
    pub(crate) stored: StoredRecord,
    pub(crate) native: crate::registry::NativeIdentity,
}
fn recorded_mode(record: &crate::registry::SessionRecord) -> Option<ProfileMode> {
    record.model.as_ref().map(|model| ProfileMode {
        id: record.mode.clone().unwrap_or_default(),
        label: None,
        model: model.clone(),
        effort: record.effort.clone(),
        service_tier: record.service_tier.clone(),
    })
}

#[allow(clippy::too_many_lines)] // Preflight the whole translated archive before publication.
pub(crate) fn import(
    config: &Config,
    registry: &Arc<Registry>,
    mut archive: DecodedArchive,
) -> Result<PreparedImport> {
    let spec = ResumeSpec::from_record(&archive.manifest.session)?;
    let matching: Vec<_> = config
        .profiles
        .iter()
        .filter(|profile| {
            profile.name == spec.profile && profile.harness.eq_ignore_ascii_case(&spec.harness)
        })
        .collect();
    let [profile] = matching.as_slice() else {
        bail!("target has no unique matching profile")
    };
    let profile = (*profile).clone();
    let provider = harness(&spec.harness)?;
    let root = crate::old_sessions::profile_config_directory(&profile, provider)?;
    directory(&root, true)?;
    let project = resolve_project(config, &spec)?;
    let cwd = config
        .resolve_launch_directory(&project.join(spec.cwd.strip_prefix(&spec.project_root)?))
        .context("target cwd is outside configured roots")?;
    directory(&cwd, false)?;
    let relative = match provider {
        ResumeHarness::Claude => PathBuf::from("projects")
            .join(encoded_project(&cwd))
            .join(format!("{}.jsonl", spec.native_id)),
        ResumeHarness::Codex => {
            let parts: Vec<_> = spec.native_path.components().collect();
            ensure!(
                parts.len() == 5
                    && parts[0].as_os_str() == "sessions"
                    && parts[1..4].iter().enumerate().all(|(index, part)| {
                        let value = part.as_os_str().to_string_lossy();
                        value.len() == if index == 0 { 4 } else { 2 }
                            && value.bytes().all(|ch| ch.is_ascii_digit())
                    })
                    && spec.native_path.file_name().is_some_and(|name| name
                        .to_string_lossy()
                        .ends_with(&format!("{}.jsonl", spec.native_id))),
                "invalid Codex rollout location"
            );
            spec.native_path.clone()
        }
    };
    if provider == ResumeHarness::Claude {
        ensure!(
            spec.native_path
                == PathBuf::from("projects")
                    .join(encoded_project(&spec.cwd))
                    .join(format!("{}.jsonl", spec.native_id)),
            "invalid Claude native location"
        );
    }
    let main = PathBuf::from("native").join(&spec.native_path);
    let sibling = main.with_extension("");
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut total = 0_u64;
    for mut entry in archive.entries {
        let path = Path::new(&entry.path);
        let is_main = path == main;
        let target = if is_main {
            root.join(&relative)
        } else {
            ensure!(
                provider == ResumeHarness::Claude && path.starts_with(&sibling),
                "unexpected native archive entry"
            );
            root.join(relative.with_extension(""))
                .join(path.strip_prefix(&sibling)?)
        };
        if let Some(mut data) = entry.data.take() {
            if is_main
                || path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
            {
                data = translate_file(registry, &mut data.file, &spec, &project, &cwd, is_main)?;
            }
            total = total
                .checked_add(data.file.metadata()?.len())
                .context("native size overflow")?;
            ensure!(
                total <= registry.bundle_limit(),
                "translated archive exceeds cap"
            );
            files.push((target, data));
        } else {
            directories.push(target);
        }
    }
    let mode = recorded_mode(&archive.manifest.session.record);
    if let Some(mode) = &mode {
        let mut checked = profile.clone();
        checked.modes = vec![ProfileMode {
            id: "imported".to_owned(),
            ..mode.clone()
        }];
        Config {
            profiles: vec![checked],
            ..Config::default()
        }
        .validate_profiles()?;
    }
    for (path, data) in &mut files {
        preflight(path, &mut data.file, registry.bundle_limit())?;
    }
    for path in directories {
        directory(&path, true)?;
    }
    for (path, mut data) in files {
        publish(&path, &mut data.file, registry.bundle_limit())?;
    }
    archive.manifest.session.record.project.root = project.to_string_lossy().into_owned();
    archive.manifest.session.record.cwd = cwd.to_string_lossy().into_owned();
    let native = crate::registry::NativeIdentity {
        config_root: root.clone(),
        session_id: spec.native_id.clone(),
        log_path: root.join(&relative),
    };
    Ok(PreparedImport {
        directory: cwd,
        profile,
        candidate: ResumeCandidate::imported(provider, &spec.native_id)?,
        mode,
        stored: archive.manifest.session,
        native,
    })
}
fn hash_file(file: &mut File, limit: u64) -> Result<(u64, String)> {
    file.seek(SeekFrom::Start(0))?;
    let mut total = 0_u64;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(total <= limit, "native file exceeds cap");
        digest.update(&buffer[..count]);
    }
    file.seek(SeekFrom::Start(0))?;
    Ok((total, format!("{:x}", digest.finalize())))
}
fn existing(parent: &File, name: &std::ffi::OsStr, limit: u64) -> Result<Option<File>> {
    match rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let file = File::from(fd);
            let m = file.metadata()?;
            ensure!(
                m.is_file()
                    && m.nlink() == 1
                    && m.uid() == rustix::process::geteuid().as_raw()
                    && m.len() <= limit,
                "unsafe native file"
            );
            Ok(Some(file))
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn preflight(path: &Path, source: &mut File, limit: u64) -> Result<()> {
    let parent = directory(path.parent().context("missing native parent")?, true)?;
    if let Some(mut old) = existing(
        &parent,
        path.file_name().context("missing file name")?,
        limit,
    )? {
        ensure!(
            hash_file(&mut old, limit)? == hash_file(source, limit)?,
            "conflicting native file; nothing overwritten"
        );
    }
    Ok(())
}
fn publish(path: &Path, source: &mut File, limit: u64) -> Result<()> {
    let parent = directory(path.parent().context("missing native parent")?, true)?;
    let name = path.file_name().context("missing file name")?;
    if let Some(mut old) = existing(&parent, name, limit)? {
        ensure!(
            hash_file(&mut old, limit)? == hash_file(source, limit)?,
            "conflicting native file"
        );
        return Ok(());
    }
    let staging = format!(".import-{}", crate::tmux::new_session_key()?);
    let result = (|| {
        let mut dest = File::from(rustix::fs::openat(
            &parent,
            &staging,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        source.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut source.take(limit + 1), &mut dest)?;
        ensure!(dest.metadata()?.len() <= limit, "native output exceeds cap");
        dest.sync_all()?;
        match rustix::fs::linkat(&parent, &staging, &parent, name, AtFlags::empty()) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {
                let mut old = existing(&parent, name, limit)?.context("native disappeared")?;
                ensure!(
                    hash_file(&mut old, limit)? == hash_file(source, limit)?,
                    "conflicting native file"
                );
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    })();
    let _ = rustix::fs::unlinkat(&parent, &staging, AtFlags::empty());
    parent.sync_all()?;
    result
}
pub(crate) fn server_id() -> Option<String> {
    Tmux::output(["display-message", "-p", "#{pid}"])
        .ok()
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
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
    } else if let Some(remote) = remote.strip_prefix("ssh://") {
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

fn resolve_project(config: &Config, manifest: &ResumeSpec) -> Result<PathBuf> {
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
            ensure!(
                !branch.starts_with('-')
                    && branch.len() <= 4096
                    && !branch.chars().any(char::is_control),
                "invalid source branch"
            );
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
            &format!("https://{normalized}"),
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::registry::{NativeIdentity, ProjectPosition, SessionRecord};
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    const NATIVE: &str = "019a06d9-8341-7654-8abc-0123456789ab";

    pub(crate) fn seed(
        registry: &Arc<Registry>,
        root: &Path,
        cwd: &Path,
        key: &str,
        provider: ResumeHarness,
    ) -> Result<File> {
        let native_root = root.join("source-native");
        let relative = match provider {
            ResumeHarness::Claude => {
                PathBuf::from(format!("projects/{}/{NATIVE}.jsonl", encoded_project(cwd)))
            }
            ResumeHarness::Codex => {
                PathBuf::from(format!("sessions/2026/09/30/rollout-{NATIVE}.jsonl"))
            }
        };
        let path = native_root.join(&relative);
        fs::create_dir_all(path.parent().unwrap())?;
        let row = match provider {
            ResumeHarness::Claude => {
                serde_json::json!({"sessionId":NATIVE,"cwd":cwd,"type":"user","message":{"content":"keep /home/ryan/source body"}})
            }
            ResumeHarness::Codex => {
                serde_json::json!({"type":"session_meta","payload":{"id":NATIVE,"cwd":cwd,"body":"keep /home/ryan/source body"}})
            }
        };
        fs::write(&path, format!("{row}\n"))?;
        if provider == ResumeHarness::Claude {
            let sibling = path.with_extension("");
            fs::create_dir_all(sibling.join("empty"))?;
            fs::write(sibling.join("subagent.jsonl"), format!("{row}\n"))?;
            fs::write(sibling.join("opaque.bin"), [0, 1, 255, 2])?;
        }
        let mut session = crate::control::test_session("a4-recorder", "%1", "working");
        session.path = cwd.to_owned();
        session.profile = "hd".into();
        session.session_key = Some(key.into());
        session.agent = match provider {
            ResumeHarness::Claude => crate::status::AgentKind::Claude,
            ResumeHarness::Codex => crate::status::AgentKind::Codex,
        };
        session.agent_pid = None;
        let stored = StoredRecord {
            record: SessionRecord {
                session_key: key.into(),
                profile: "hd".into(),
                harness: match provider {
                    ResumeHarness::Claude => "claude",
                    ResumeHarness::Codex => "codex",
                }
                .into(),
                created_ms: 1000,
                project: ProjectPosition {
                    root: cwd.to_string_lossy().into_owned(),
                    remote: Some("https://github.com/ryanmurf/atmux.git".into()),
                    branch: Some("main".into()),
                    ..ProjectPosition::default()
                },
                mode: Some("opus".into()),
                model: Some(
                    if provider == ResumeHarness::Claude {
                        "opus"
                    } else {
                        "gpt-5"
                    }
                    .into(),
                ),
                effort: Some("high".into()),
                ..SessionRecord::default()
            },
            ..StoredRecord::default()
        };
        registry.commit_resume(
            stored,
            &session,
            NativeIdentity {
                config_root: native_root,
                session_id: NATIVE.into(),
                log_path: path,
            },
            0,
            1000,
        )?;
        Ok(registry.capture_bundle(key)?.0)
    }
    struct Fixture {
        root: PathBuf,
        registry: Arc<Registry>,
        config: Config,
    }
    impl Fixture {
        fn new(provider: ResumeHarness) -> Self {
            let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "atmux-a4-translation-{}",
                crate::tmux::new_session_key().unwrap()
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let project = root.join("Users/ryan/IdeaProjects/atmux");
            fs::create_dir_all(&project).unwrap();
            for args in [
                ["init", "-q", "-b", "main"].as_slice(),
                [
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/ryanmurf/atmux.git",
                ]
                .as_slice(),
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
            let native = root.join("target-native");
            fs::create_dir(&native).unwrap();
            let mut config = Config::default();
            config.node.id = "mac".into();
            config.general.project_roots = vec![project.parent().unwrap().to_owned()];
            config.general.favorite_dirs.clear();
            config.profiles = vec![AgentProfile {
                name: "hd".into(),
                harness: match provider {
                    ResumeHarness::Claude => "claude",
                    ResumeHarness::Codex => "codex",
                }
                .into(),
                env: BTreeMap::from([(
                    match provider {
                        ResumeHarness::Claude => "CLAUDE_CONFIG_DIR",
                        ResumeHarness::Codex => "CODEX_HOME",
                    }
                    .into(),
                    native.to_string_lossy().into_owned(),
                )]),
                command: "fixture".into(),
                args: Vec::new(),
                inherit_discovered: false,
                claude_relaunch_permissions: None,
                modes: Vec::new(),
            }];
            config.registry.enabled = true;
            config.registry.directory = Some(root.join("registry"));
            let registry = Registry::open(&config.registry, "source").unwrap().unwrap();
            Self {
                root,
                registry,
                config,
            }
        }
        fn archive(&self, provider: ResumeHarness) -> DecodedArchive {
            let file = seed(
                &self.registry,
                &self.root,
                Path::new("/home/ryan/IdeaProjects/atmux"),
                &crate::tmux::new_session_key().unwrap(),
                provider,
            )
            .unwrap();
            decode(&self.registry, file).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn a3_claude_archive_translates_home_metadata_siblings_and_preserves_bodies() {
        let fixture = Fixture::new(ResumeHarness::Claude);
        let archive = fixture.archive(ResumeHarness::Claude);
        let prepared = import(&fixture.config, &fixture.registry, archive).unwrap();
        let log = fs::read_to_string(&prepared.native.log_path).unwrap();
        assert!(log.contains("Users/ryan/IdeaProjects/atmux"));
        assert!(log.contains("keep /home/ryan/source body"));
        let sibling = prepared.native.log_path.with_extension("");
        assert!(sibling.join("empty").is_dir());
        assert_eq!(
            fs::read(sibling.join("opaque.bin")).unwrap(),
            [0, 1, 255, 2]
        );
        assert!(
            fs::read_to_string(sibling.join("subagent.jsonl"))
                .unwrap()
                .contains("Users/ryan/IdeaProjects/atmux")
        );
        import(
            &fixture.config,
            &fixture.registry,
            fixture.archive(ResumeHarness::Claude),
        )
        .unwrap();
        fs::write(&prepared.native.log_path, "different").unwrap();
        assert!(
            import(
                &fixture.config,
                &fixture.registry,
                fixture.archive(ResumeHarness::Claude)
            )
            .is_err()
        );
    }
    #[test]
    fn a3_codex_archive_translates_only_session_meta_cwd_and_keeps_date() {
        let fixture = Fixture::new(ResumeHarness::Codex);
        let prepared = import(
            &fixture.config,
            &fixture.registry,
            fixture.archive(ResumeHarness::Codex),
        )
        .unwrap();
        assert!(
            prepared
                .native
                .log_path
                .ends_with(format!("sessions/2026/09/30/rollout-{NATIVE}.jsonl"))
        );
        let log = fs::read_to_string(prepared.native.log_path).unwrap();
        assert!(log.contains("Users/ryan/IdeaProjects/atmux"));
        assert!(log.contains("keep /home/ryan/source body"));
    }
    #[test]
    fn a3_import_refuses_missing_profile_wrong_branch_and_symlink_store() {
        let fixture = Fixture::new(ResumeHarness::Claude);
        let mut config = fixture.config.clone();
        config.profiles.clear();
        assert!(
            import(
                &config,
                &fixture.registry,
                fixture.archive(ResumeHarness::Claude)
            )
            .is_err()
        );
        let mut archive = fixture.archive(ResumeHarness::Claude);
        archive.manifest.session.record.project.branch = Some("other".into());
        assert!(import(&fixture.config, &fixture.registry, archive).is_err());
        let target = fixture.root.join("target-native");
        fs::remove_dir(&target).unwrap();
        symlink(fixture.root.join("source-native"), &target).unwrap();
        assert!(
            import(
                &fixture.config,
                &fixture.registry,
                fixture.archive(ResumeHarness::Claude)
            )
            .is_err()
        );
    }
    #[test]
    fn a3_decode_rejects_oversize_traversal_and_checksum_damage() {
        let fixture = Fixture::new(ResumeHarness::Claude);
        let staged = StagedFile::new(&fixture.registry).unwrap();
        staged
            .file
            .set_len(fixture.registry.bundle_limit() + 1)
            .unwrap();
        assert!(decode(&fixture.registry, staged.file.try_clone().unwrap()).is_err());
        assert!(!valid_relative(Path::new("native/../escape")));
        assert!(!valid_relative(Path::new("/native/escape")));
        let mut archive = fixture.archive(ResumeHarness::Claude);
        archive.manifest.files[0].sha256 = "0".repeat(64);
        // Rebuild an otherwise valid tar with a damaged declared checksum.
        let staged = StagedFile::new(&fixture.registry).unwrap();
        let gzip = flate2::write::GzEncoder::new(
            staged.file.try_clone().unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gzip);
        let bytes = serde_json::to_vec(&archive.manifest).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        tar.append_data(&mut header, "manifest.json", bytes.as_slice())
            .unwrap();
        for entry in &mut archive.entries {
            let item = archive
                .manifest
                .files
                .iter()
                .find(|item| item.path == entry.path)
                .unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o600);
            header.set_size(item.bytes);
            if item.directory {
                header.set_entry_type(tar::EntryType::Directory);
            }
            header.set_cksum();
            if let Some(data) = &mut entry.data {
                data.file.seek(SeekFrom::Start(0)).unwrap();
                tar.append_data(&mut header, &entry.path, &mut data.file)
                    .unwrap();
            } else {
                tar.append_data(&mut header, &entry.path, std::io::empty())
                    .unwrap();
            }
        }
        tar.into_inner().unwrap().finish().unwrap();
        assert!(decode(&fixture.registry, staged.file.try_clone().unwrap()).is_err());
    }
}
