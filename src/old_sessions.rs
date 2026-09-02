//! Bounded discovery of native Claude and Codex conversations for Quick Launch.
//!
//! Browser callers never choose a session-store path and never receive a native
//! session id. The control plane selects one configured profile, validates the
//! requested project directory, then uses this module to inspect that profile's
//! native store. A launch must run discovery again and resolve the browser's
//! opaque handle against the fresh result before appending fixed resume args.

use std::{
    env, fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::{Result, bail};
use serde_json::Value;

use crate::config::AgentProfile;

const CLAUDE_CONFIG_KEY: &str = "CLAUDE_CONFIG_DIR";
const CODEX_CONFIG_KEY: &str = "CODEX_HOME";
const HEAD_BYTES: u64 = 64 * 1024;
const TAIL_BYTES: u64 = 256 * 1024;
const MAX_PREVIEW_CHARS: usize = 140;
const MAX_FUTURE_MTIME: Duration = Duration::from_secs(5 * 60);

/// Limits applied independently to every old-session discovery request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DiscoveryLimits {
    pub(crate) entries: usize,
    pub(crate) files: usize,
    pub(crate) total_bytes: u64,
    pub(crate) results: usize,
    pub(crate) age: Duration,
    pub(crate) elapsed: Duration,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self {
            entries: 8_192,
            files: 2_048,
            total_bytes: 32 * 1024 * 1024,
            results: 20,
            age: Duration::from_secs(90 * 24 * 60 * 60),
            elapsed: Duration::from_secs(2),
        }
    }
}

/// Harness-specific identity retained only inside the owning atmux node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResumeHarness {
    Claude,
    Codex,
}

impl ResumeHarness {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// One revalidated native session. Its provider id must never be serialized.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ResumeCandidate {
    harness: ResumeHarness,
    session_id: String,
    pub(crate) updated_ms: u64,
    pub(crate) preview: String,
}

impl std::fmt::Debug for ResumeCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResumeCandidate")
            .field("harness", &self.harness)
            .field("session_id", &"[redacted]")
            .field("updated_ms", &self.updated_ms)
            .field("preview", &self.preview)
            .finish()
    }
}

impl ResumeCandidate {
    pub(crate) const fn harness(&self) -> ResumeHarness {
        self.harness
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    #[cfg(test)]
    pub(crate) fn fixture(harness: ResumeHarness, session_id: &str) -> Self {
        Self {
            harness,
            session_id: session_id.to_owned(),
            updated_ms: 1,
            preview: "fixture".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Discovery {
    pub(crate) sessions: Vec<ResumeCandidate>,
    pub(crate) truncated: bool,
}

#[derive(Debug)]
struct ScanBudget {
    limits: DiscoveryLimits,
    started: Instant,
    entries: usize,
    files: usize,
    bytes: u64,
    truncated: bool,
}

impl ScanBudget {
    fn new(limits: DiscoveryLimits) -> Result<Self> {
        if limits.entries == 0
            || limits.files == 0
            || limits.total_bytes < HEAD_BYTES
            || limits.results == 0
            || limits.age.is_zero()
            || limits.elapsed.is_zero()
        {
            bail!("old-session discovery limits are invalid");
        }
        Ok(Self {
            limits,
            started: Instant::now(),
            entries: 0,
            files: 0,
            bytes: 0,
            truncated: false,
        })
    }

    fn entry(&mut self) -> bool {
        if self.entries >= self.limits.entries || self.started.elapsed() > self.limits.elapsed {
            self.truncated = true;
            return false;
        }
        self.entries += 1;
        true
    }

    fn file(&mut self, requested_bytes: u64) -> bool {
        if self.files >= self.limits.files
            || self.bytes.saturating_add(requested_bytes) > self.limits.total_bytes
            || self.started.elapsed() > self.limits.elapsed
        {
            self.truncated = true;
            return false;
        }
        self.files += 1;
        self.bytes = self.bytes.saturating_add(requested_bytes);
        true
    }
}

/// Discovers recent native sessions for one configured profile and exact folder.
///
/// The caller is expected to pass the result of `Config::resolve_launch_directory`.
/// This function canonicalizes again so a folder replaced between validation and
/// discovery cannot broaden the match.
///
/// # Errors
///
/// Returns an error for an unsupported harness, unsafe/missing native store,
/// unsafe launch folder, or invalid work limits.
pub(crate) fn discover(
    profile: &AgentProfile,
    directory: &Path,
    limits: DiscoveryLimits,
) -> Result<Discovery> {
    let directory = canonical_real_directory(directory, "launch directory is unsafe")?;
    let harness = profile_harness(profile)?;
    let config = profile_config_directory(profile, harness)?;
    let config = canonical_real_directory(&config, "agent session store is unavailable")?;
    let mut budget = ScanBudget::new(limits)?;
    let mut sessions = match harness {
        ResumeHarness::Claude => discover_claude(&config, &directory, &mut budget)?,
        ResumeHarness::Codex => discover_codex(&config, &directory, &mut budget)?,
    };
    sessions.sort_by(|left, right| {
        right
            .updated_ms
            .cmp(&left.updated_ms)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    sessions.dedup_by(|left, right| {
        left.harness == right.harness && left.session_id == right.session_id
    });
    if sessions.len() > limits.results {
        sessions.truncate(limits.results);
        budget.truncated = true;
    }
    Ok(Discovery {
        sessions,
        truncated: budget.truncated,
    })
}

/// Fixed native arguments for a server-resolved candidate.
///
/// This deliberately recognizes only Claude and Codex. A configured wrapper
/// whose harness is something else cannot turn an opaque handle into arbitrary
/// command arguments.
///
/// # Errors
///
/// Returns an error when the server-derived native session id is malformed.
pub(crate) fn resume_arguments(candidate: &ResumeCandidate) -> Result<Vec<String>> {
    if !valid_session_id(candidate.session_id()) {
        bail!("saved agent session is invalid");
    }
    Ok(match candidate.harness() {
        ResumeHarness::Claude => vec!["--resume".to_owned(), candidate.session_id.clone()],
        ResumeHarness::Codex => vec!["resume".to_owned(), candidate.session_id.clone()],
    })
}

fn profile_harness(profile: &AgentProfile) -> Result<ResumeHarness> {
    match profile.harness.to_ascii_lowercase().as_str() {
        "claude" => Ok(ResumeHarness::Claude),
        "codex" => Ok(ResumeHarness::Codex),
        _ => bail!("saved-session launch is supported only for Claude and Codex profiles"),
    }
}

fn profile_config_directory(profile: &AgentProfile, harness: ResumeHarness) -> Result<PathBuf> {
    let (key, fallback) = match harness {
        ResumeHarness::Claude => (CLAUDE_CONFIG_KEY, ".claude"),
        ResumeHarness::Codex => (CODEX_CONFIG_KEY, ".codex"),
    };
    if let Some(config) = profile.env.get(key) {
        let config = PathBuf::from(config);
        if !config.is_absolute()
            || config
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            bail!("agent session store binding is not canonical");
        }
        let canonical = canonical_real_directory(&config, "agent session store is unavailable")?;
        if canonical != config {
            bail!("agent session store binding is not canonical");
        }
        return Ok(canonical);
    }
    if profile.command != harness.as_str() {
        bail!("saved-session discovery requires an explicit profile session store");
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| anyhow::anyhow!("agent session store is unavailable"))?;
    Ok(home.join(fallback))
}

fn discover_claude(
    config: &Path,
    directory: &Path,
    budget: &mut ScanBudget,
) -> Result<Vec<ResumeCandidate>> {
    let projects = safe_child_directory(config, "projects")?;
    let Some(projects) = projects else {
        return Ok(Vec::new());
    };
    let encoded = encode_claude_project(directory);
    let Some(project) = safe_child_directory(&projects, &encoded)? else {
        return Ok(Vec::new());
    };
    let mut candidates = Vec::new();
    let entries = fs::read_dir(project)
        .map_err(|_| anyhow::anyhow!("agent session store could not be read"))?;
    for result in entries {
        if !budget.entry() {
            break;
        }
        let Ok(entry) = result else {
            continue;
        };
        let path = entry.path();
        let Some((metadata, modified_ms)) = eligible_session_file(&entry, budget.limits.age) else {
            continue;
        };
        let Some(session_id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if !valid_session_id(session_id) {
            continue;
        }
        let requested = metadata
            .len()
            .min(HEAD_BYTES)
            .saturating_add(metadata.len().min(TAIL_BYTES));
        if !budget.file(requested) {
            break;
        }
        let Some(head) = read_head(&path, HEAD_BYTES) else {
            continue;
        };
        if !claude_head_matches(&head, directory, session_id) {
            continue;
        }
        let preview = read_tail(&path, TAIL_BYTES)
            .as_deref()
            .and_then(claude_preview)
            .unwrap_or_else(|| "Previous Claude conversation".to_owned());
        candidates.push(ResumeCandidate {
            harness: ResumeHarness::Claude,
            session_id: session_id.to_owned(),
            updated_ms: modified_ms,
            preview,
        });
    }
    Ok(candidates)
}

fn discover_codex(
    config: &Path,
    directory: &Path,
    budget: &mut ScanBudget,
) -> Result<Vec<ResumeCandidate>> {
    let Some(sessions) = safe_child_directory(config, "sessions")? else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    collect_codex_files(&sessions, &sessions, 0, budget, &mut files)?;
    let mut candidates = Vec::new();
    for (path, metadata, modified_ms) in files {
        let requested = metadata
            .len()
            .min(HEAD_BYTES)
            .saturating_add(metadata.len().min(TAIL_BYTES));
        if !budget.file(requested) {
            break;
        }
        let Some(head) = read_head(&path, HEAD_BYTES) else {
            continue;
        };
        let Some(session_id) = codex_session_id(&path, &head, directory) else {
            continue;
        };
        let preview = read_tail(&path, TAIL_BYTES)
            .as_deref()
            .and_then(codex_preview)
            .unwrap_or_else(|| "Previous Codex conversation".to_owned());
        candidates.push(ResumeCandidate {
            harness: ResumeHarness::Codex,
            session_id,
            updated_ms: modified_ms,
            preview,
        });
    }
    Ok(candidates)
}

fn collect_codex_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    budget: &mut ScanBudget,
    files: &mut Vec<(PathBuf, fs::Metadata, u64)>,
) -> Result<()> {
    if depth > 5 || budget.truncated {
        budget.truncated |= depth > 5;
        return Ok(());
    }
    let entries = fs::read_dir(directory)
        .map_err(|_| anyhow::anyhow!("agent session store could not be read"))?;
    for result in entries {
        if !budget.entry() {
            break;
        }
        let Ok(entry) = result else {
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            let canonical = match entry.path().canonicalize() {
                Ok(path) if path.starts_with(root) => path,
                _ => continue,
            };
            collect_codex_files(root, &canonical, depth + 1, budget, files)?;
            continue;
        }
        if !kind.is_file()
            || !entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case("jsonl"))
        {
            continue;
        }
        let Some((metadata, modified_ms)) = eligible_session_file(&entry, budget.limits.age) else {
            continue;
        };
        files.push((entry.path(), metadata, modified_ms));
        if files.len() >= budget.limits.files {
            budget.truncated = true;
            break;
        }
    }
    Ok(())
}

fn eligible_session_file(entry: &fs::DirEntry, max_age: Duration) -> Option<(fs::Metadata, u64)> {
    let kind = entry.file_type().ok()?;
    if kind.is_symlink() || !kind.is_file() {
        return None;
    }
    let metadata = entry.metadata().ok()?;
    if metadata.len() == 0 {
        return None;
    }
    let modified = metadata.modified().ok()?;
    let now = std::time::SystemTime::now();
    if modified > now.checked_add(MAX_FUTURE_MTIME)? || now.duration_since(modified).ok()? > max_age
    {
        return None;
    }
    let modified_ms = modified.duration_since(UNIX_EPOCH).ok()?.as_millis();
    Some((metadata, u64::try_from(modified_ms).ok()?))
}

fn claude_head_matches(bytes: &[u8], directory: &Path, session_id: &str) -> bool {
    json_lines(bytes).take(16).any(|value| {
        value
            .get("sessionId")
            .and_then(Value::as_str)
            .is_none_or(|id| id == session_id)
            && value
                .get("cwd")
                .and_then(Value::as_str)
                .is_some_and(|cwd| exact_directory(cwd, directory))
    })
}

fn codex_session_id(path: &Path, bytes: &[u8], directory: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    json_lines(bytes).take(16).find_map(|value| {
        if value.get("type").and_then(Value::as_str) != Some("session_meta")
            || value.pointer("/payload/source").and_then(Value::as_str) != Some("cli")
            || value
                .pointer("/payload/thread_source")
                .and_then(Value::as_str)
                .is_some_and(|source| source != "user")
            || !value
                .pointer("/payload/cwd")
                .and_then(Value::as_str)
                .is_some_and(|cwd| exact_directory(cwd, directory))
        {
            return None;
        }
        let id = value.pointer("/payload/id").and_then(Value::as_str)?;
        (valid_session_id(id) && file_name.ends_with(&format!("{id}.jsonl"))).then(|| id.to_owned())
    })
}

fn claude_preview(bytes: &[u8]) -> Option<String> {
    json_lines(bytes)
        .filter_map(|value| {
            if value.pointer("/message/role").and_then(Value::as_str) != Some("user") {
                return None;
            }
            let content = value.pointer("/message/content")?;
            let text = content.as_str().map(str::to_owned).or_else(|| {
                let parts = content.as_array()?.iter().filter_map(|block| {
                    (block.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| block.get("text").and_then(Value::as_str))
                        .flatten()
                });
                Some(parts.collect::<Vec<_>>().join(" "))
            })?;
            safe_preview(&text)
        })
        .next_back()
}

fn codex_preview(bytes: &[u8]) -> Option<String> {
    let response_preview = json_lines(bytes)
        .filter_map(|value| {
            if value.get("type").and_then(Value::as_str) != Some("response_item")
                || value.pointer("/payload/type").and_then(Value::as_str) != Some("message")
                || value.pointer("/payload/role").and_then(Value::as_str) != Some("user")
            {
                return None;
            }
            let text = value
                .pointer("/payload/content")?
                .as_array()?
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("input_text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            safe_preview(&text)
        })
        .next_back();
    response_preview.or_else(|| {
        json_lines(bytes)
            .filter_map(|value| {
                (value.get("type").and_then(Value::as_str) == Some("event_msg")
                    && value.pointer("/payload/type").and_then(Value::as_str)
                        == Some("user_message"))
                .then(|| value.pointer("/payload/message").and_then(Value::as_str))
                .flatten()
                .and_then(safe_preview)
            })
            .next_back()
    })
}

fn safe_preview(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    if trimmed.is_empty() || is_injected_context(trimmed) {
        return None;
    }
    let mut words = Vec::new();
    let mut inside_private_key = false;
    for line in trimmed.lines() {
        let lower = line.to_ascii_lowercase();
        let starts_private_key =
            lower.contains("-----begin ") && lower.contains("private key-----");
        let ends_private_key = lower.contains("-----end ") && lower.contains("private key-----");
        if inside_private_key {
            inside_private_key = !ends_private_key;
            continue;
        }
        if starts_private_key {
            words.push("[redacted]".to_owned());
            inside_private_key = !ends_private_key;
        } else if sensitive_line(&lower) {
            words.push("[redacted]".to_owned());
        } else {
            words.extend(line.split_whitespace().map(str::to_owned));
        }
    }
    let flat = words.join(" ");
    if flat.is_empty() {
        return None;
    }
    Some(truncate_chars(&flat, MAX_PREVIEW_CHARS))
}

fn sensitive_line(lower: &str) -> bool {
    lower.contains("bearer ")
        || [
            "authorization:",
            "proxy-authorization:",
            "cookie:",
            "set-cookie:",
            "authorization=",
            "password=",
            "password:",
            "\"password\"",
            "client_secret",
            "client-secret",
            "api_key",
            "api_token",
            "api-key",
            "apikey",
            "access_token",
            "access-token",
            "refresh_token",
            "refresh-token",
            "secret_access_key",
            "secret-access-key",
            "private_key",
            "private-key",
            "token=",
            "token:",
            "\"token\"",
            "\"secret\"",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn is_injected_context(text: &str) -> bool {
    text.starts_with("<environment_context>")
        || text.starts_with("<permissions instructions>")
        || text.starts_with("<collaboration_mode>")
        || text.starts_with("<plugins_instructions>")
        || text.starts_with("<skills_instructions>")
        || text.starts_with("<system-reminder>")
        || text.starts_with("# AGENTS.md instructions\n\n<INSTRUCTIONS>")
}

fn truncate_chars(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let mut output = chars.by_ref().take(max).collect::<String>();
    if chars.next().is_some() {
        output.push('…');
    }
    output
}

fn exact_directory(value: &str, directory: &Path) -> bool {
    let stored = Path::new(value);
    stored.is_absolute()
        && stored
            .canonicalize()
            .is_ok_and(|stored| stored == directory)
}

fn safe_child_directory(root: &Path, child: &str) -> Result<Option<PathBuf>> {
    let candidate = root.join(child);
    let metadata = match fs::symlink_metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("agent session store could not be inspected"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("agent session store is unsafe");
    }
    let canonical = candidate
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("agent session store could not be inspected"))?;
    if !canonical.starts_with(root) {
        bail!("agent session store escaped its configured root");
    }
    Ok(Some(canonical))
}

fn canonical_real_directory(path: &Path, message: &'static str) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!(message);
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| anyhow::anyhow!(message))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(message);
    }
    path.canonicalize().map_err(|_| anyhow::anyhow!(message))
}

fn encode_claude_project(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn valid_session_id(value: &str) -> bool {
    value.len() == 36
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        && value.as_bytes().get(8) == Some(&b'-')
        && value.as_bytes().get(13) == Some(&b'-')
        && value.as_bytes().get(18) == Some(&b'-')
        && value.as_bytes().get(23) == Some(&b'-')
}

fn read_head(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(limit)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

fn read_tail(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let mut file = fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let read = size.min(limit);
    file.seek(SeekFrom::Start(size.saturating_sub(read))).ok()?;
    let mut bytes = Vec::with_capacity(usize::try_from(read).ok()?);
    file.take(read).read_to_end(&mut bytes).ok()?;
    if size > read {
        let start = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1);
        bytes.drain(..start);
    }
    Some(bytes)
}

fn json_lines(bytes: &[u8]) -> impl DoubleEndedIterator<Item = Value> + '_ {
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, time::Duration};

    use super::*;

    const CLAUDE_ID: &str = "11111111-2222-4333-8444-555555555555";
    const CODEX_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    struct Fixture {
        root: PathBuf,
        project: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = env::temp_dir().join(format!("atmux-old-session-{label}-{nonce}"));
            let project = root.join("work/project");
            fs::create_dir_all(&project).unwrap();
            Self {
                root: root.canonicalize().unwrap(),
                project: project.canonicalize().unwrap(),
            }
        }

        fn profile(harness: &str, config: &Path) -> AgentProfile {
            let key = if harness == "claude" {
                CLAUDE_CONFIG_KEY
            } else {
                CODEX_CONFIG_KEY
            };
            AgentProfile {
                name: "fixture".to_owned(),
                harness: harness.to_owned(),
                command: harness.to_owned(),
                args: Vec::new(),
                env: BTreeMap::from([(key.to_owned(), config.display().to_string())]),
                inherit_discovered: false,
                claude_relaunch_permissions: None,
                modes: Vec::new(),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_limits() -> DiscoveryLimits {
        DiscoveryLimits {
            entries: 100,
            files: 20,
            total_bytes: 2 * 1024 * 1024,
            results: 10,
            age: Duration::from_secs(24 * 60 * 60),
            elapsed: Duration::from_secs(2),
        }
    }

    #[test]
    fn discovers_exact_claude_folder_and_redacts_preview() {
        let fixture = Fixture::new("claude");
        let config = fixture.root.join("claude-config");
        let sessions = config
            .join("projects")
            .join(encode_claude_project(&fixture.project));
        fs::create_dir_all(&sessions).unwrap();
        let source = format!(
            "{{\"sessionId\":\"{CLAUDE_ID}\",\"cwd\":{},\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"first\"}}}}\n{{\"sessionId\":\"{CLAUDE_ID}\",\"cwd\":{},\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"API_KEY=do-not-return\"}}}}\n",
            serde_json::to_string(&fixture.project.display().to_string()).unwrap(),
            serde_json::to_string(&fixture.project.display().to_string()).unwrap(),
        );
        fs::write(sessions.join(format!("{CLAUDE_ID}.jsonl")), source).unwrap();

        let found = discover(
            &Fixture::profile("claude", &config),
            &fixture.project,
            test_limits(),
        )
        .unwrap();
        assert_eq!(found.sessions.len(), 1);
        assert_eq!(found.sessions[0].harness(), ResumeHarness::Claude);
        assert_eq!(found.sessions[0].preview, "[redacted]");
        assert!(!format!("{:?}", found.sessions[0]).contains(CLAUDE_ID));
        assert_eq!(
            resume_arguments(&found.sessions[0]).unwrap(),
            ["--resume", CLAUDE_ID]
        );
    }

    #[test]
    fn discovers_codex_cli_session_and_rejects_other_folder() {
        let fixture = Fixture::new("codex");
        let config = fixture.root.join("codex-config");
        let sessions = config.join("sessions/2026/08/18");
        fs::create_dir_all(&sessions).unwrap();
        let source = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{CODEX_ID}\",\"cwd\":{},\"source\":\"cli\",\"thread_source\":\"user\"}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"Fix the mobile layout\"}}]}}}}\n",
            serde_json::to_string(&fixture.project.display().to_string()).unwrap(),
        );
        fs::write(
            sessions.join(format!("rollout-test-{CODEX_ID}.jsonl")),
            source,
        )
        .unwrap();
        let profile = Fixture::profile("codex", &config);

        let found = discover(&profile, &fixture.project, test_limits()).unwrap();
        assert_eq!(found.sessions.len(), 1);
        assert_eq!(found.sessions[0].preview, "Fix the mobile layout");
        assert_eq!(
            resume_arguments(&found.sessions[0]).unwrap(),
            ["resume", CODEX_ID]
        );

        let other = fixture.root.join("work/other");
        fs::create_dir(&other).unwrap();
        let other = other.canonicalize().unwrap();
        assert!(
            discover(&profile, &other, test_limits())
                .unwrap()
                .sessions
                .is_empty()
        );
    }

    #[test]
    fn unsupported_harness_and_symlink_store_fail_closed() {
        let fixture = Fixture::new("closed");
        let config = fixture.root.join("config");
        fs::create_dir(&config).unwrap();
        let unsupported = Fixture::profile("grok", &config);
        assert!(discover(&unsupported, &fixture.project, test_limits()).is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&config, fixture.root.join("linked-config")).unwrap();
            let linked = Fixture::profile("claude", &fixture.root.join("linked-config"));
            assert!(discover(&linked, &fixture.project, test_limits()).is_err());
        }
    }

    #[test]
    fn non_bare_wrappers_never_fall_back_to_the_default_home_store() {
        let fixture = Fixture::new("wrapper-fallback");
        for (harness, command) in [
            ("claude", "/opt/agents/claude-max"),
            ("claude", "/opt/agents/claude"),
            ("codex", "/opt/agents/codex-work"),
            ("codex", "/opt/agents/codex"),
        ] {
            let mut profile = Fixture::profile(harness, &fixture.root);
            profile.command = command.to_owned();
            profile.env.clear();
            let error = profile_config_directory(&profile, profile_harness(&profile).unwrap())
                .unwrap_err()
                .to_string();
            assert!(error.contains("explicit profile session store"), "{error}");
        }
    }

    #[test]
    fn bare_native_commands_may_use_the_default_home_store() {
        for (harness, expected) in [("claude", ".claude"), ("codex", ".codex")] {
            let mut profile = Fixture::profile(harness, Path::new("/unused"));
            profile.command = harness.to_owned();
            profile.env.clear();
            let directory =
                profile_config_directory(&profile, profile_harness(&profile).unwrap()).unwrap();
            assert!(directory.ends_with(expected));
        }
    }

    #[test]
    fn loaded_default_profiles_keep_explicit_stores_after_command_resolution() {
        let fixture = Fixture::new("loaded-defaults");
        let path = fixture.root.join("config.toml");
        fs::write(&path, crate::config::DEFAULT_CONFIG).unwrap();
        let (config, _) = crate::config::Config::load(Some(&path)).unwrap();
        let home = PathBuf::from(env::var_os("HOME").unwrap());
        for (harness, key, expected) in [
            ("claude", CLAUDE_CONFIG_KEY, home.join(".claude")),
            ("codex", CODEX_CONFIG_KEY, home.join(".codex")),
        ] {
            let profile = config
                .profiles
                .iter()
                .find(|profile| profile.harness == harness && profile.name == "Default")
                .unwrap();
            assert_eq!(profile.env.get(key).map(PathBuf::from), Some(expected));
        }
    }

    #[test]
    fn wrapper_store_binding_must_be_an_existing_canonical_directory() {
        let fixture = Fixture::new("canonical-binding");
        let config = fixture.root.join("claude-max");
        let intermediate = fixture.root.join("intermediate");
        fs::create_dir(&config).unwrap();
        fs::create_dir(&intermediate).unwrap();
        let config = config.canonicalize().unwrap();
        let mut profile = Fixture::profile("claude", &config);
        profile.command = "/opt/agents/claude-max".to_owned();
        assert_eq!(
            profile_config_directory(&profile, ResumeHarness::Claude).unwrap(),
            config
        );

        profile.env.insert(
            CLAUDE_CONFIG_KEY.to_owned(),
            intermediate
                .join("../claude-max")
                .to_string_lossy()
                .into_owned(),
        );
        assert!(profile_config_directory(&profile, ResumeHarness::Claude).is_err());

        profile.env.insert(
            CLAUDE_CONFIG_KEY.to_owned(),
            "relative/claude-max".to_owned(),
        );
        assert!(profile_config_directory(&profile, ResumeHarness::Claude).is_err());

        profile.env.insert(
            CLAUDE_CONFIG_KEY.to_owned(),
            fixture.root.join("missing").to_string_lossy().into_owned(),
        );
        assert!(profile_config_directory(&profile, ResumeHarness::Claude).is_err());
    }

    #[test]
    fn result_and_scan_bounds_are_enforced() {
        let fixture = Fixture::new("bounds");
        let config = fixture.root.join("claude-config");
        let sessions = config
            .join("projects")
            .join(encode_claude_project(&fixture.project));
        fs::create_dir_all(&sessions).unwrap();
        for index in 0..3 {
            let id = format!("00000000-0000-4000-8000-{index:012x}");
            let source = format!(
                "{{\"sessionId\":\"{id}\",\"cwd\":{},\"message\":{{\"role\":\"user\",\"content\":\"message {index}\"}}}}\n",
                serde_json::to_string(&fixture.project.display().to_string()).unwrap(),
            );
            fs::write(sessions.join(format!("{id}.jsonl")), source).unwrap();
        }
        let mut limits = test_limits();
        limits.results = 2;
        let found = discover(
            &Fixture::profile("claude", &config),
            &fixture.project,
            limits,
        )
        .unwrap();
        assert_eq!(found.sessions.len(), 2);
        assert!(found.truncated);
    }
}
