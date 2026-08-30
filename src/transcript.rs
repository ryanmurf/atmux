//! Bounded, read-only views of the conversation logs written by agent CLIs.
//!
//! The browser never chooses a filesystem path.  atmux derives candidates from
//! the selected tmux pane's agent kind, working directory, and non-sensitive
//! launch label. It returns human/assistant messages plus bounded tool calls
//! and results; system prompts, environment records, and reasoning stay
//! server-side.

use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    env, fs,
    hash::{Hash, Hasher},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::{status::AgentKind, tmux::Session};

const MAX_LOG_TAIL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CLAUDE_METADATA_BYTES: u64 = 64 * 1024;
const MAX_MESSAGE_BYTES: usize = 128 * 1024;
const MAX_TRANSCRIPT_BYTES: usize = 768 * 1024;
const MAX_MESSAGES: usize = 240;
const MAX_PARSE_MESSAGES: usize = MAX_MESSAGES * 2;
const MAX_ITEM_ID_BYTES: usize = 512;
const MAX_TIMESTAMP_BYTES: usize = 128;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_NESTED_JSON_DEPTH: usize = 8;
const MAX_CLAUDE_ROOTS: usize = 64;
// Claude writes its PID metadata after CLI initialization and authentication;
// that can lag the OS process start noticeably on macOS and networked auth.
// The exact live PID + cwd check remains the primary identity boundary.
const MAX_PROCESS_LOG_START_SKEW_MS: u64 = 120_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TranscriptMessage {
    pub id: String,
    pub role: String,
    #[serde(default = "default_message_kind")]
    pub kind: String,
    pub markdown: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

fn default_message_kind() -> String {
    "message".to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Transcript {
    pub available: bool,
    pub source: String,
    pub content_hash: String,
    pub changed: bool,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<TranscriptMessage>>,
}

impl Transcript {
    #[must_use]
    pub fn unavailable(source: &str) -> Self {
        Self {
            available: false,
            source: source.to_owned(),
            content_hash: String::new(),
            changed: false,
            truncated: false,
            messages: Some(Vec::new()),
        }
    }
}

/// Current context usage taken from the exact native log mapped to one live
/// pane. This stays server-side: the stable digest is used only for durable
/// owner-local de-duplication and reveals neither a provider session id nor a
/// filesystem path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeContext {
    pub(crate) session_fingerprint: String,
    pub(crate) input_tokens: u64,
    /// A native compact record exists after the latest usage record. Until the
    /// CLI writes a post-compact usage sample, another compact must fail closed.
    pub(crate) reset_pending: bool,
}

/// Reads native context usage for the one log provably owned by `session`.
///
/// Terminal text is never consulted. Missing identity, ambiguous mappings,
/// unsupported harnesses, malformed usage, overflow, and bounded-tail gaps all
/// return `None`, which makes automatic mutation fail closed.
#[must_use]
pub(crate) fn native_context(session: &Session) -> Option<NativeContext> {
    if !matches!(session.agent, AgentKind::Claude | AgentKind::Codex) {
        return None;
    }
    let path = locate(session)?;
    let log = read_bounded_tail(&path).ok()?;
    let (input_tokens, usage_index, compact_index) =
        parse_native_context_tail(session.agent, &log)?;
    let canonical = path.canonicalize().ok()?;
    let mut digest = Sha256::new();
    digest.update(match session.agent {
        AgentKind::Claude => b"claude".as_slice(),
        AgentKind::Codex => b"codex".as_slice(),
        AgentKind::Other => return None,
    });
    digest.update([0]);
    digest.update(canonical.to_string_lossy().as_bytes());
    Some(NativeContext {
        session_fingerprint: format!("{:x}", digest.finalize()),
        input_tokens,
        reset_pending: compact_index.is_some_and(|index| index > usage_index),
    })
}

fn parse_native_context_tail(
    agent: AgentKind,
    log: &LogTail,
) -> Option<(u64, usize, Option<usize>)> {
    // If the bounded read excluded any newer bytes, the apparent latest usage
    // is not current enough to authorize an automatic mutation.
    (!log.read_capped)
        .then(|| parse_native_context(agent, &log.bytes))
        .flatten()
}

fn parse_native_context(agent: AgentKind, bytes: &[u8]) -> Option<(u64, usize, Option<usize>)> {
    let mut latest_usage = None;
    let mut latest_compact = None;
    let physical = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let newest_nonempty = physical.iter().rposition(|line| !line.is_empty());
    let mut parsed_index = 0_usize;
    for (physical_index, line) in physical.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_slice(line) {
            Ok(value) => value,
            // A writer may have exposed its newest JSONL record between
            // writes. Reusing the preceding high count could compact the
            // wrong current context, so a nonempty physical tail must parse.
            Err(_) if Some(physical_index) == newest_nonempty => return None,
            // Older malformed records do not hide a later complete, current
            // native usage sample.
            Err(_) => continue,
        };
        let index = parsed_index;
        parsed_index += 1;
        match agent {
            AgentKind::Claude => {
                if value.get("type").and_then(Value::as_str) == Some("system")
                    && value.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
                {
                    latest_compact = Some(index);
                }
                let Some(message) = value.get("message") else {
                    continue;
                };
                let assistant = value.get("type").and_then(Value::as_str) == Some("assistant")
                    || message.get("role").and_then(Value::as_str) == Some("assistant");
                if !assistant {
                    continue;
                }
                let usage = message.get("usage")?;
                let input = usage.get("input_tokens").and_then(Value::as_u64)?;
                let cache_creation = usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64)?;
                let cache_read = usage
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)?;
                let total = input
                    .checked_add(cache_creation)
                    .and_then(|total| total.checked_add(cache_read))?;
                latest_usage = Some((total, index));
            }
            AgentKind::Codex => {
                if value.get("type").and_then(Value::as_str) == Some("compacted") {
                    latest_compact = Some(index);
                    continue;
                }
                if value.get("type").and_then(Value::as_str) != Some("event_msg")
                    || value.pointer("/payload/type").and_then(Value::as_str) != Some("token_count")
                {
                    continue;
                }
                // Codex's native last_token_usage is the current request's
                // context input. total_token_usage is conversation-cumulative
                // and cached_input_tokens is already included in input_tokens.
                let input = value
                    .pointer("/payload/info/last_token_usage/input_tokens")
                    .and_then(Value::as_u64)?;
                latest_usage = Some((input, index));
            }
            AgentKind::Other => return None,
        }
    }
    latest_usage.map(|(tokens, index)| (tokens, index, latest_compact))
}

/// Reads the newest bounded conversation view for one local tmux session.
///
/// No path supplied by an HTTP caller reaches this function. Candidate files
/// are confined to the current user's Claude/Codex data directories.
///
/// # Errors
///
/// Returns an error when an exactly matched transcript cannot be read. Missing
/// or ambiguous transcript metadata is represented as an unavailable view.
pub fn read(session: &Session, known_hash: Option<&str>) -> Result<Transcript> {
    let source = match session.agent {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Other => return Ok(Transcript::unavailable("terminal")),
    };
    let Some(path) = locate(session) else {
        return Ok(Transcript::unavailable(source));
    };
    let bytes = read_bounded_tail(&path)?;
    let content_hash = transcript_hash(source, &path, &bytes.bytes);
    if known_hash == Some(content_hash.as_str()) {
        return Ok(Transcript {
            available: true,
            source: source.to_owned(),
            content_hash,
            changed: false,
            truncated: bytes.starts_mid_line || bytes.read_capped,
            messages: None,
        });
    }
    let (messages, mut truncated) = match session.agent {
        AgentKind::Claude => parse_claude(&bytes),
        AgentKind::Codex => parse_codex(&bytes),
        AgentKind::Other => (Vec::new(), false),
    };
    let (messages, bounded) = bound_messages(messages);
    truncated |= bounded || bytes.starts_mid_line || bytes.read_capped;
    Ok(Transcript {
        available: true,
        source: source.to_owned(),
        content_hash,
        changed: true,
        truncated,
        messages: Some(messages),
    })
}

#[derive(Debug)]
struct LogTail {
    bytes: Vec<u8>,
    starts_mid_line: bool,
    read_capped: bool,
}

fn read_bounded_tail(path: &Path) -> Result<LogTail> {
    let mut file = fs::File::open(path).context("failed to open the selected agent log")?;
    let before = file
        .metadata()
        .context("failed to inspect the selected agent log")?;
    let length = before.len();
    let tail = read_bounded(&mut file, length)?;
    let after = file
        .metadata()
        .context("failed to re-inspect the selected agent log")?;
    if after.len() != length || after.modified().ok() != before.modified().ok() {
        bail!("the selected agent log changed during its bounded snapshot");
    }
    Ok(tail)
}

fn read_bounded<R: Read + Seek>(mut reader: R, sampled_length: u64) -> Result<LogTail> {
    let start = sampled_length.saturating_sub(MAX_LOG_TAIL_BYTES);
    reader
        .seek(SeekFrom::Start(start))
        .context("failed to seek in the selected agent log")?;
    let capacity = usize::try_from(sampled_length - start)
        .unwrap_or(4 * 1024 * 1024)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(capacity);
    reader
        .take(MAX_LOG_TAIL_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("failed to read the selected agent log")?;
    let read_capped = u64::try_from(bytes.len()).is_ok_and(|length| length > MAX_LOG_TAIL_BYTES);
    if read_capped {
        bytes.truncate(usize::try_from(MAX_LOG_TAIL_BYTES).unwrap_or(4 * 1024 * 1024));
    }
    let starts_mid_line = start > 0;
    if starts_mid_line {
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        } else {
            bytes.clear();
        }
    }
    Ok(LogTail {
        bytes,
        starts_mid_line,
        read_capped,
    })
}

fn locate(session: &Session) -> Option<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from)?;
    match session.agent {
        // Claude rewrites sessions/<pid>.json on /clear. Re-read it on every
        // poll so a long-lived process can never leave us on the old log.
        AgentKind::Claude => locate_claude(session, &home),
        // Codex keeps the current rollout open. Resolve the native process file
        // descriptors on every poll; /new closes the old writer and opens the
        // new one without changing the tmux pane or process start time.
        AgentKind::Codex => locate_codex(session, &codex_root(&session.launch_command, &home)),
        AgentKind::Other => None,
    }
}

fn locate_claude(session: &Session, home: &Path) -> Option<PathBuf> {
    claude_resume_target_in_home(session, home).map(|target| target.log_path)
}

/// The server-only data required to replace a stopped Claude process with the
/// current launcher while retaining its native conversation.  Neither field
/// is suitable for a browser response: the config root identifies an account
/// boundary and the session id can resume its conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeResumeTarget {
    pub(crate) config_dir: PathBuf,
    pub(crate) session_id: String,
    log_path: PathBuf,
}

/// Exact owner-local saved conversation required for a maintenance relaunch.
/// This is never serialized: config roots and native ids remain owner-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeResumeTarget {
    pub(crate) config_dir: PathBuf,
    pub(crate) session_id: String,
    pub(crate) session_fingerprint: String,
}

/// Resolves the exact native saved conversation currently held open by a pane.
/// Claude uses its PID metadata; Codex uses its one open, cwd-matching rollout.
#[must_use]
pub(crate) fn native_resume_target(session: &Session) -> Option<NativeResumeTarget> {
    let home = env::var_os("HOME").map(PathBuf::from)?;
    let (config_dir, session_id, log_path) = match session.agent {
        AgentKind::Claude => {
            let target = claude_resume_target_in_home(session, &home)?;
            (target.config_dir, target.session_id, target.log_path)
        }
        AgentKind::Codex => {
            let config_dir = codex_root(&session.launch_command, &home);
            let log_path = locate_codex(session, &config_dir)?;
            let file_name = log_path.file_name()?.to_str()?;
            let values = first_json_values(&log_path, 8)?;
            let session_id = values.iter().find_map(|value| {
                (value.get("type").and_then(Value::as_str) == Some("session_meta"))
                    .then(|| value.pointer("/payload/id").and_then(Value::as_str))
                    .flatten()
                    .filter(|id| valid_session_id(id))
                    .filter(|id| file_name.ends_with(&format!("{id}.jsonl")))
                    .map(str::to_owned)
            })?;
            (config_dir, session_id, log_path)
        }
        AgentKind::Other => return None,
    };
    let canonical = log_path.canonicalize().ok()?;
    let mut digest = Sha256::new();
    digest.update(session.agent.to_string().to_ascii_lowercase().as_bytes());
    digest.update([0]);
    digest.update(canonical.to_string_lossy().as_bytes());
    Some(NativeResumeTarget {
        config_dir,
        session_id,
        session_fingerprint: format!("{:x}", digest.finalize()),
    })
}

/// Finds the one Claude session log provably owned by this live pane.  The
/// same identity checks used for transcript reads make a resume target safe:
/// exact process PID, working directory, near-equal process start, a bounded
/// metadata file, and a regular project log under one non-symlink config root.
#[must_use]
pub(crate) fn claude_resume_target(session: &Session) -> Option<ClaudeResumeTarget> {
    let home = env::var_os("HOME").map(PathBuf::from)?;
    claude_resume_target_in_home(session, &home)
}

fn claude_resume_target_in_home(session: &Session, home: &Path) -> Option<ClaudeResumeTarget> {
    if session.agent != AgentKind::Claude {
        return None;
    }
    let pid = session.agent_pid?;
    let mut roots = claude_roots(&session.launch_command, home)?;
    let matching = roots
        .drain(..)
        .filter_map(|root| claude_resume_target_in_root(session, pid, &root))
        .collect::<Vec<_>>();
    (matching.len() == 1).then(|| matching[0].clone())
}

fn claude_resume_target_in_root(
    session: &Session,
    pid: u32,
    root: &Path,
) -> Option<ClaudeResumeTarget> {
    let metadata_path = root.join("sessions").join(format!("{pid}.json"));
    if !regular_file_within(&metadata_path, root) {
        return None;
    }
    let metadata = read_bounded_json(&metadata_path, MAX_CLAUDE_METADATA_BYTES)?;
    if metadata.get("pid").and_then(Value::as_u64) != Some(u64::from(pid))
        || metadata
            .get("cwd")
            .and_then(Value::as_str)
            .is_none_or(|cwd| Path::new(cwd) != session.path)
        || !starts_close_enough(
            session.agent_started_ms,
            metadata.get("startedAt").and_then(Value::as_u64),
        )
    {
        return None;
    }
    let session_id = metadata
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| valid_session_id(value))?;
    let projects = root.join("projects");
    let encoded = encode_claude_project(&session.path);
    let log_path = projects.join(encoded).join(format!("{session_id}.jsonl"));
    regular_file_within(&log_path, root).then_some(ClaudeResumeTarget {
        config_dir: root.to_owned(),
        session_id: session_id.to_owned(),
        log_path,
    })
}

fn claude_roots(label: &str, home: &Path) -> Option<Vec<PathBuf>> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    let preferred = claude_root(label, home);
    if regular_directory(&preferred) && seen.insert(preferred.clone()) {
        roots.push(preferred);
    }
    let mut discovered = Vec::new();
    for entry in fs::read_dir(home).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name != ".claude" && !name.strip_prefix(".claude-").is_some_and(safe_profile_leaf) {
            continue;
        }
        if discovered.len() == MAX_CLAUDE_ROOTS {
            return None;
        }
        discovered.push(entry.path());
    }
    discovered.sort();
    for root in discovered {
        if regular_directory(&root) && seen.insert(root.clone()) {
            roots.push(root);
        }
    }
    Some(roots)
}

fn regular_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_dir() && !metadata.file_type().is_symlink())
}

fn read_bounded_json(path: &Path, limit: u64) -> Option<Value> {
    let mut bytes = Vec::with_capacity(usize::try_from(limit).ok()?.saturating_add(1));
    fs::File::open(path)
        .ok()?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (u64::try_from(bytes.len()).ok()? <= limit)
        .then(|| serde_json::from_slice(&bytes).ok())
        .flatten()
}

fn locate_codex(session: &Session, root: &Path) -> Option<PathBuf> {
    let pid = session.agent_pid?;
    let paths = process_open_paths(pid).ok()?;
    select_codex_rollout(paths, root, &session.path)
}

fn select_codex_rollout(
    paths: impl IntoIterator<Item = PathBuf>,
    root: &Path,
    cwd: &Path,
) -> Option<PathBuf> {
    let mut seen = HashSet::new();
    let matching = paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .filter(|path| regular_file_within(path, root))
        .filter(|path| codex_log_matches(path, cwd))
        .collect::<Vec<_>>();
    if matching.len() == 1 {
        matching.into_iter().next()
    } else {
        // Showing no structured transcript is safer than showing a different
        // same-directory conversation or a concurrently open child thread.
        None
    }
}

#[cfg(target_os = "linux")]
fn process_open_paths(pid: u32) -> Result<Vec<PathBuf>> {
    let directory = PathBuf::from(format!("/proc/{pid}/fd"));
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("failed to inspect the selected Codex process"),
    };
    Ok(entries
        .flatten()
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .filter(|path| path.is_absolute())
        .collect())
}

#[cfg(not(target_os = "linux"))]
fn process_open_paths(pid: u32) -> Result<Vec<PathBuf>> {
    // LaunchAgents use a deliberately narrow PATH that normally omits
    // /usr/sbin. Use macOS's fixed system binary so Codex transcript lookup
    // works in the Aqua service context and cannot be redirected through PATH.
    #[cfg(target_os = "macos")]
    let lsof = "/usr/sbin/lsof";
    #[cfg(not(target_os = "macos"))]
    let lsof = "lsof";
    let output = std::process::Command::new(lsof)
        .args(["-a", "-p", &pid.to_string(), "-Fn"])
        .output()
        .context("failed to inspect the selected Codex process")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect())
}

fn claude_root(label: &str, home: &Path) -> PathBuf {
    if let Some(value) = label.strip_prefix("CLAUDE_CONFIG_DIR=")
        && let Some((directory, _)) = value.split_once(" · ")
    {
        let path = PathBuf::from(directory.trim_end_matches(" (unexpanded)"));
        if path.is_absolute() && safe_claude_root(&path, home) {
            return path;
        }
    }
    let executable = label.rsplit(" · ").next().unwrap_or(label);
    if let Some(name) = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        && let Some(profile) = name.strip_prefix("claude-")
        && safe_profile_leaf(profile)
    {
        let candidate = home.join(format!(".claude-{profile}"));
        if candidate.is_dir() {
            return candidate;
        }
    }
    home.join(".claude")
}

fn codex_root(label: &str, home: &Path) -> PathBuf {
    if let Some(value) = label.strip_prefix("CODEX_HOME=")
        && let Some((directory, _)) = value.split_once(" · ")
    {
        let path = PathBuf::from(directory.trim_end_matches(" (unexpanded)"));
        if path.parent() == Some(home)
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name == ".codex" || name.strip_prefix(".codex-").is_some_and(safe_profile_leaf)
                })
        {
            return path;
        }
    }
    home.join(".codex")
}

fn safe_claude_root(path: &Path, home: &Path) -> bool {
    path.parent() == Some(home)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name == ".claude" || name.strip_prefix(".claude-").is_some_and(safe_profile_leaf)
            })
}

fn safe_profile_leaf(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
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

fn codex_log_matches(path: &Path, cwd: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    first_json_values(path, 8).is_some_and(|values| {
        values.iter().any(|value| {
            value.get("type").and_then(Value::as_str) == Some("session_meta")
                && value
                    .pointer("/payload/id")
                    .and_then(Value::as_str)
                    .filter(|id| valid_session_id(id))
                    .is_some_and(|id| file_name.ends_with(&format!("{id}.jsonl")))
                && value.pointer("/payload/source").and_then(Value::as_str) == Some("cli")
                && value
                    .pointer("/payload/thread_source")
                    .and_then(Value::as_str)
                    .is_none_or(|source| source == "user")
                && value
                    .pointer("/payload/cwd")
                    .and_then(Value::as_str)
                    .is_some_and(|value| Path::new(value) == cwd)
        })
    })
}

fn starts_close_enough(process: Option<u64>, log: Option<u64>) -> bool {
    match (process, log) {
        (Some(process), Some(log)) => process.abs_diff(log) <= MAX_PROCESS_LOG_START_SKEW_MS,
        _ => false,
    }
}

fn valid_session_id(value: &str) -> bool {
    value.len() == 36
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

fn regular_file_within(path: &Path, root: &Path) -> bool {
    if fs::symlink_metadata(path).is_err()
        || fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return false;
    }
    path.canonicalize()
        .ok()
        .zip(root.canonicalize().ok())
        .is_some_and(|(path, root)| path.starts_with(root) && path.is_file())
}

fn first_json_values(path: &Path, limit: usize) -> Option<Vec<Value>> {
    let mut source = Vec::with_capacity(64 * 1024);
    fs::File::open(path)
        .ok()?
        .take(64 * 1024)
        .read_to_end(&mut source)
        .ok()?;
    Some(
        source
            .split(|byte| *byte == b'\n')
            .take(limit)
            .filter_map(|line| serde_json::from_slice(line).ok())
            .collect(),
    )
}

fn parse_claude(log: &LogTail) -> (Vec<TranscriptMessage>, bool) {
    let mut messages = Vec::new();
    for value in json_lines(&log.bytes) {
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let role = value.pointer("/message/role").and_then(Value::as_str);
        let id = value.get("uuid").and_then(Value::as_str);
        let timestamp = value.get("timestamp").and_then(Value::as_str);
        let Some(content) = value.pointer("/message/content") else {
            continue;
        };
        match role {
            Some("user") => {
                if let Some(markdown) =
                    claude_user_text(&value).filter(|text| !text.trim().is_empty())
                    && !is_injected_user_context(&markdown)
                {
                    push_message(&mut messages, "user", markdown, id, timestamp);
                }
                if let Some(blocks) = content.as_array() {
                    for (index, block) in blocks.iter().enumerate() {
                        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                            continue;
                        }
                        let output = tool_output_text(block.get("content"));
                        let call_id = block.get("tool_use_id").and_then(Value::as_str);
                        attach_tool_output(
                            &mut messages,
                            call_id,
                            output,
                            derived_id(id, "tool-result", index).as_deref(),
                            timestamp,
                        );
                    }
                }
            }
            Some("assistant") => {
                if let Some(markdown) = content.as_str().map(str::to_owned) {
                    push_message(&mut messages, "assistant", markdown, id, timestamp);
                    continue;
                }
                let Some(blocks) = content.as_array() else {
                    continue;
                };
                for (index, block) in blocks.iter().enumerate() {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(markdown) = block
                                .get("text")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .filter(|text| !text.trim().is_empty())
                            {
                                let block_id = derived_id(id, "text", index);
                                push_message(
                                    &mut messages,
                                    "assistant",
                                    markdown,
                                    block_id.as_deref(),
                                    timestamp,
                                );
                            }
                        }
                        Some("tool_use") => {
                            let call_id = block.get("id").and_then(Value::as_str);
                            let fallback = derived_id(id, "tool", index);
                            push_tool(
                                &mut messages,
                                block.get("name").and_then(Value::as_str).unwrap_or("Tool"),
                                tool_input_text(block.get("input")),
                                call_id.or(fallback.as_deref()),
                                timestamp,
                            );
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    (messages, false)
}

fn derived_id(base: Option<&str>, kind: &str, index: usize) -> Option<String> {
    let base = base.filter(|value| value.len() <= MAX_ITEM_ID_BYTES)?;
    let suffix = format!(":{kind}:{index}");
    if base.len().saturating_add(suffix.len()) > MAX_ITEM_ID_BYTES {
        return None;
    }
    let mut id = String::with_capacity(base.len() + suffix.len());
    id.push_str(base);
    id.push_str(&suffix);
    Some(id)
}

fn claude_user_text(value: &Value) -> Option<String> {
    let content = value.pointer("/message/content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    content_text(Some(content), "text")
}

fn parse_codex(log: &LogTail) -> (Vec<TranscriptMessage>, bool) {
    let mut messages = Vec::new();
    for value in json_lines(&log.bytes) {
        if value.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let payload_type = value.pointer("/payload/type").and_then(Value::as_str);
        let timestamp = value.get("timestamp").and_then(Value::as_str);
        match payload_type {
            Some("message") => {
                let role = value.pointer("/payload/role").and_then(Value::as_str);
                let block_type = match role {
                    Some("user") => "input_text",
                    Some("assistant") => "output_text",
                    _ => continue,
                };
                let Some(markdown) = content_text(value.pointer("/payload/content"), block_type)
                    .filter(|text| !text.trim().is_empty())
                else {
                    continue;
                };
                if role == Some("user") && is_injected_user_context(&markdown) {
                    continue;
                }
                push_message(
                    &mut messages,
                    role.unwrap_or_default(),
                    markdown,
                    value.pointer("/payload/id").and_then(Value::as_str),
                    timestamp,
                );
            }
            Some("function_call" | "custom_tool_call") => {
                let input = if payload_type == Some("function_call") {
                    tool_input_text(value.pointer("/payload/arguments"))
                } else {
                    tool_input_text(value.pointer("/payload/input"))
                };
                push_tool(
                    &mut messages,
                    value
                        .pointer("/payload/name")
                        .and_then(Value::as_str)
                        .unwrap_or("Tool"),
                    input,
                    value
                        .pointer("/payload/call_id")
                        .or_else(|| value.pointer("/payload/id"))
                        .and_then(Value::as_str),
                    timestamp,
                );
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                attach_tool_output(
                    &mut messages,
                    value.pointer("/payload/call_id").and_then(Value::as_str),
                    tool_output_text(value.pointer("/payload/output")),
                    value.pointer("/payload/id").and_then(Value::as_str),
                    timestamp,
                );
            }
            _ => {}
        }
    }
    if !messages.iter().any(|message| message.kind == "message") {
        for value in json_lines(&log.bytes) {
            if value.get("type").and_then(Value::as_str) != Some("event_msg") {
                continue;
            }
            let event_type = value.pointer("/payload/type").and_then(Value::as_str);
            let role = match event_type {
                Some("user_message") => "user",
                Some("agent_message") => "assistant",
                _ => continue,
            };
            let Some(markdown) = value
                .pointer("/payload/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|text| !text.trim().is_empty())
            else {
                continue;
            };
            if role == "user" && is_injected_user_context(&markdown) {
                continue;
            }
            push_message(
                &mut messages,
                role,
                markdown,
                None,
                value.get("timestamp").and_then(Value::as_str),
            );
        }
    }
    (messages, false)
}

fn content_text(content: Option<&Value>, block_type: &str) -> Option<String> {
    let blocks = content?.as_array()?;
    let text = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(block_type))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

fn tool_input_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return sanitize_or_redact_string(text, 0);
    }
    serialize_redacted_json(value, 0, true)
}

fn tool_output_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return sanitize_or_redact_string(text, 0);
    }
    if let Some(items) = value.as_array() {
        let mut output = String::new();
        for item in items {
            let rendered = item
                .as_str()
                .and_then(|text| sanitize_or_redact_string(text, 0))
                .or_else(|| {
                    item.get("text")
                        .and_then(Value::as_str)
                        .and_then(|text| sanitize_or_redact_string(text, 0))
                })
                .or_else(|| serialize_redacted_json(item, 0, true));
            if let Some(rendered) = rendered
                && !append_bounded_tool_segment(&mut output, &rendered)
            {
                break;
            }
        }
        return (!output.is_empty()).then_some(output);
    }
    serialize_redacted_json(value, 0, true)
}

fn append_bounded_tool_segment(output: &mut String, segment: &str) -> bool {
    const TRUNCATION_MARKER: &str = "\n…tool output truncated by atmux…";
    let content_limit = MAX_MESSAGE_BYTES.saturating_sub(TRUNCATION_MARKER.len());
    let separator = if output.is_empty() { "" } else { "\n\n" };
    if separator.len().saturating_add(segment.len()) <= content_limit.saturating_sub(output.len()) {
        output.push_str(separator);
        output.push_str(segment);
        return true;
    }

    let mut remaining = content_limit.saturating_sub(output.len());
    if remaining >= separator.len() {
        output.push_str(separator);
        remaining -= separator.len();
    }
    let mut boundary = remaining.min(segment.len());
    while !segment.is_char_boundary(boundary) {
        boundary -= 1;
    }
    output.push_str(&segment[..boundary]);
    output.push_str(TRUNCATION_MARKER);
    false
}

fn redact_json_at_depth(value: &Value, depth: usize) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    let value = if sensitive_key(key) {
                        Value::String("[redacted]".to_owned())
                    } else {
                        redact_json_at_depth(value, depth)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| redact_json_at_depth(value, depth))
                .collect(),
        ),
        Value::String(value) => Value::String(redact_string(value, depth)),
        _ => value.clone(),
    }
}

fn redact_string(value: &str, depth: usize) -> String {
    let trimmed = value.trim_start();
    let looks_like_json = trimmed.starts_with('{') || trimmed.starts_with('[');
    if looks_like_json {
        if depth >= MAX_NESTED_JSON_DEPTH {
            return "[redacted deeply nested JSON]".to_owned();
        }
        if value.len() > MAX_MESSAGE_BYTES {
            return "[redacted oversized JSON string]".to_owned();
        }
        if let Ok(nested) = serde_json::from_str::<Value>(value) {
            return serialize_redacted_json(&nested, depth + 1, false)
                .unwrap_or_else(|| "[redacted invalid nested JSON]".to_owned());
        }
    }
    sanitize_tool_text(value).unwrap_or_default()
}

fn sanitize_or_redact_string(value: &str, depth: usize) -> Option<String> {
    let redacted = redact_string(value, depth);
    (!redacted.trim().is_empty()).then_some(redacted)
}

struct LimitedJsonWriter {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl LimitedJsonWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(16 * 1024)),
            limit,
            truncated: false,
        }
    }
}

impl Write for LimitedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        if bytes.len() > remaining {
            self.bytes.extend_from_slice(&bytes[..remaining]);
            self.truncated = true;
            return Err(std::io::Error::other(
                "tool JSON exceeded its display limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_redacted_json(value: &Value, depth: usize, pretty: bool) -> Option<String> {
    const TRUNCATION_RESERVE: usize = 64;
    let redacted = redact_json_at_depth(value, depth);
    let mut writer = LimitedJsonWriter::new(MAX_MESSAGE_BYTES - TRUNCATION_RESERVE);
    let result = if pretty {
        serde_json::to_writer_pretty(&mut writer, &redacted)
    } else {
        serde_json::to_writer(&mut writer, &redacted)
    };
    if result.is_err() && !writer.truncated {
        return None;
    }
    let mut output = String::from_utf8_lossy(&writer.bytes).into_owned();
    if writer.truncated {
        while !output.is_char_boundary(output.len()) {
            output.pop();
        }
        output.push_str("\n…tool JSON truncated by atmux…");
    }
    Some(output)
}

fn sensitive_key(key: &str) -> bool {
    let compact = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    compact.contains("password")
        || compact.contains("secret")
        || compact.contains("credential")
        || compact.contains("authorization")
        || compact.contains("cookie")
        || compact.contains("privatekey")
        || compact.contains("apikey")
        || compact.ends_with("token")
}

fn sanitize_tool_text(text: &str) -> Option<String> {
    let mut sanitized = Vec::new();
    let mut inside_private_key = false;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let begins_private_key =
            lower.contains("-----begin ") && lower.contains("private key-----");
        let ends_private_key = lower.contains("-----end ") && lower.contains("private key-----");
        if inside_private_key {
            if ends_private_key {
                inside_private_key = false;
            }
            continue;
        }
        if begins_private_key {
            sanitized.push("[redacted private key]".to_owned());
            inside_private_key = !ends_private_key;
        } else if sensitive_tool_line(&lower) {
            sanitized.push("[redacted sensitive tool line]".to_owned());
        } else {
            sanitized.push(line.to_owned());
        }
    }
    let sanitized = sanitized.join("\n");
    (!sanitized.trim().is_empty()).then_some(sanitized)
}

fn sensitive_tool_line(lower: &str) -> bool {
    lower.contains("bearer ")
        || [
            "authorization:",
            "proxy-authorization:",
            "cookie:",
            "set-cookie:",
            "password=",
            "password:",
            "\"password\"",
            "client_secret",
            "client-secret",
            "api_key",
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
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn is_injected_user_context(text: &str) -> bool {
    let value = text.trim_start();
    value.starts_with("<environment_context>")
        || value.starts_with("<permissions instructions>")
        || value.starts_with("<collaboration_mode>")
        || value.starts_with("<plugins_instructions>")
        || value.starts_with("<skills_instructions>")
        || value.starts_with("<system-reminder>")
        || value.starts_with("# AGENTS.md instructions\n\n<INSTRUCTIONS>")
}

fn push_message(
    messages: &mut Vec<TranscriptMessage>,
    role: &str,
    markdown: String,
    id: Option<&str>,
    timestamp: Option<&str>,
) {
    let markdown = truncate_utf8(markdown, MAX_MESSAGE_BYTES);
    let id = bounded_id(id, || format!("message-{}", messages.len()));
    if messages.last().is_some_and(|message| message.id == id) {
        return;
    }
    messages.push(TranscriptMessage {
        id,
        role: role.to_owned(),
        kind: default_message_kind(),
        markdown,
        tool_name: None,
        tool_input: None,
        tool_output: None,
        timestamp: timestamp.map(|value| truncate_plain(value, MAX_TIMESTAMP_BYTES)),
    });
    cap_parse_messages(messages);
}

fn push_tool(
    messages: &mut Vec<TranscriptMessage>,
    name: &str,
    input: Option<String>,
    id: Option<&str>,
    timestamp: Option<&str>,
) {
    let id = bounded_id(id, || format!("tool-{}", messages.len()));
    if messages.last().is_some_and(|message| message.id == id) {
        return;
    }
    messages.push(TranscriptMessage {
        id,
        role: "tool".to_owned(),
        kind: "tool".to_owned(),
        markdown: String::new(),
        tool_name: Some(truncate_plain(name, MAX_TOOL_NAME_BYTES)),
        tool_input: input.map(|value| truncate_utf8(value, MAX_MESSAGE_BYTES)),
        tool_output: None,
        timestamp: timestamp.map(|value| truncate_plain(value, MAX_TIMESTAMP_BYTES)),
    });
    cap_parse_messages(messages);
}

fn attach_tool_output(
    messages: &mut Vec<TranscriptMessage>,
    call_id: Option<&str>,
    output: Option<String>,
    fallback_id: Option<&str>,
    timestamp: Option<&str>,
) {
    let Some(output) = output else {
        return;
    };
    let output = truncate_utf8(output, MAX_MESSAGE_BYTES);
    if let Some(message) = call_id
        .filter(|value| value.len() <= MAX_ITEM_ID_BYTES)
        .and_then(|call_id| {
            messages
                .iter_mut()
                .rev()
                .find(|message| message.kind == "tool" && message.id == call_id)
        })
    {
        message.tool_output = Some(output);
        return;
    }
    messages.push(TranscriptMessage {
        id: bounded_id(fallback_id.or(call_id), || {
            format!("tool-result-{}", messages.len())
        }),
        role: "tool".to_owned(),
        kind: "tool".to_owned(),
        markdown: String::new(),
        tool_name: Some("Tool result".to_owned()),
        tool_input: None,
        tool_output: Some(output),
        timestamp: timestamp.map(|value| truncate_plain(value, MAX_TIMESTAMP_BYTES)),
    });
    cap_parse_messages(messages);
}

fn bounded_id(id: Option<&str>, fallback: impl FnOnce() -> String) -> String {
    id.filter(|value| value.len() <= MAX_ITEM_ID_BYTES)
        .map_or_else(fallback, ToOwned::to_owned)
}

fn truncate_plain(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut boundary = limit;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_owned()
}

fn cap_parse_messages(messages: &mut Vec<TranscriptMessage>) {
    if messages.len() > MAX_PARSE_MESSAGES {
        let discard = messages.len() - MAX_MESSAGES;
        messages.drain(..discard);
    }
}

fn truncate_utf8(mut value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut boundary = limit;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str("\n\n…message truncated by atmux…");
    value
}

fn bound_messages(mut messages: Vec<TranscriptMessage>) -> (Vec<TranscriptMessage>, bool) {
    let mut total = 2usize;
    let mut keep_from = messages.len();
    for (kept, (index, message)) in messages.iter().enumerate().rev().enumerate() {
        let Ok(item_bytes) = serde_json::to_vec(message).map(|bytes| bytes.len()) else {
            break;
        };
        let separator = usize::from(kept > 0);
        if kept == MAX_MESSAGES
            || total.saturating_add(separator).saturating_add(item_bytes) > MAX_TRANSCRIPT_BYTES
        {
            break;
        }
        total += separator + item_bytes;
        keep_from = index;
    }
    let removed = keep_from > 0;
    if removed {
        messages.drain(..keep_from);
    }
    (messages, removed)
}

fn json_lines(bytes: &[u8]) -> impl Iterator<Item = Value> + '_ {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice(line).ok())
}

fn transcript_hash(source: &str, path: &Path, bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    path.file_name().hash(&mut hasher);
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_native_context_uses_latest_complete_assistant_usage_and_reset_order() {
        let first = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "usage": {
                "input_tokens": 90_000,
                "cache_creation_input_tokens": 20_000,
                "cache_read_input_tokens": 91_000
            }}
        });
        let compact = serde_json::json!({"type": "system", "subtype": "compact_boundary"});
        let after = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "usage": {
                "input_tokens": 4_000,
                "cache_creation_input_tokens": 1_000,
                "cache_read_input_tokens": 2_000
            }}
        });
        let before_reset = format!("{first}\n{compact}\n");
        assert_eq!(
            parse_native_context(AgentKind::Claude, before_reset.as_bytes()),
            Some((201_000, 0, Some(1)))
        );
        let after_reset = format!("{first}\n{compact}\n{after}\n");
        assert_eq!(
            parse_native_context(AgentKind::Claude, after_reset.as_bytes()),
            Some((7_000, 2, Some(1)))
        );
    }

    #[test]
    fn codex_native_context_uses_last_request_input_not_cumulative_or_cached_twice() {
        let count = serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 9_000_000},
                "last_token_usage": {
                    "input_tokens": 200_001,
                    "cached_input_tokens": 199_000,
                    "total_tokens": 202_000
                }
            }}
        });
        let compact = serde_json::json!({
            "type": "compacted",
            "payload": {"window_number": 2, "replacement_history": []}
        });
        let bytes = format!("{{not-json}}\n{count}\n{compact}\n");
        assert_eq!(
            parse_native_context(AgentKind::Codex, bytes.as_bytes()),
            Some((200_001, 0, Some(1)))
        );
        assert_eq!(
            parse_native_context(AgentKind::Other, bytes.as_bytes()),
            None
        );
    }

    #[test]
    fn malformed_or_overflowed_native_usage_fails_closed() {
        let malformed = br#"{"type":"assistant","message":{"role":"assistant","usage":{"input_tokens":200001}}}"#;
        assert_eq!(parse_native_context(AgentKind::Claude, malformed), None);
        let healthy = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "usage": {
                "input_tokens": 100,
                "cache_creation_input_tokens": 20,
                "cache_read_input_tokens": 30
            }}
        });
        let stale_fallback = format!("{healthy}\n{}\n", String::from_utf8_lossy(malformed));
        assert_eq!(
            parse_native_context(AgentKind::Claude, stale_fallback.as_bytes()),
            None,
            "a malformed newest usage record must not reuse an older metric"
        );
        let overflow = format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "usage": {
                    "input_tokens": u64::MAX,
                    "cache_creation_input_tokens": 1,
                    "cache_read_input_tokens": 0
                }}
            })
        );
        assert_eq!(
            parse_native_context(AgentKind::Claude, overflow.as_bytes()),
            None
        );
    }

    #[test]
    fn partial_native_tail_never_reuses_stale_high_usage() {
        let claude = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "usage": {
                "input_tokens": 200_001,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }}
        });
        let codex = serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {"last_token_usage": {
                "input_tokens": 200_001
            }}}
        });
        for (agent, complete) in [
            (AgentKind::Claude, claude.to_string()),
            (AgentKind::Codex, codex.to_string()),
        ] {
            let partial = format!("{complete}\n{{\"type\":\"assistant\"");
            assert_eq!(parse_native_context(agent, partial.as_bytes()), None);
            let complete_but_malformed = format!("{complete}\n{{not-json}}\n");
            assert_eq!(
                parse_native_context(agent, complete_but_malformed.as_bytes()),
                None
            );
            let recovered = format!("{{not-json}}\n{complete}\n");
            assert!(parse_native_context(agent, recovered.as_bytes()).is_some());

            let capped = LogTail {
                bytes: format!("{complete}\n").into_bytes(),
                starts_mid_line: false,
                read_capped: true,
            };
            assert_eq!(parse_native_context_tail(agent, &capped), None);
        }
    }

    fn fixture_root(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "atmux-transcript-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn session(agent: AgentKind, path: PathBuf, pid: u32, started: u64) -> Session {
        Session {
            name: "fixture".to_owned(),
            attached: false,
            windows: 1,
            activity: 0,
            window_index: 0,
            pane_index: 0,
            pane_id: "%1".to_owned(),
            pane_pid: pid,
            pane_identity: format!("pane-v1-{}", "a".repeat(64)),
            agent_pid: Some(pid),
            agent_started_ms: Some(started),
            path,
            command: agent.to_string().to_lowercase(),
            launch_command: agent.to_string().to_lowercase(),
            title: String::new(),
            content: String::new(),
            content_hash: 0,
            agent,
            profile: "Default".to_owned(),
            resume_lease: None,
            systemd_scope: None,
            memory_max_bytes: None,
            status: crate::status::AgentStatus::Waiting,
        }
    }

    fn tail(source: &str) -> LogTail {
        LogTail {
            bytes: source.as_bytes().to_vec(),
            starts_mid_line: false,
            read_capped: false,
        }
    }

    #[test]
    fn claude_keeps_messages_and_bounded_tool_records_without_reasoning() {
        let source = concat!(
            r#"{"type":"user","uuid":"u1","timestamp":"one","message":{"role":"user","content":"please **fix** it"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","timestamp":"two","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private"},{"type":"text","text":"Done.\n\n```rust\nfn main() {}\n```"},{"type":"tool_use","id":"call-1","name":"Bash","input":{"command":"echo ok","api_key":"hide me"}}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"tool","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"ok"}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"meta","message":{"role":"user","content":"<system-reminder>hidden</system-reminder>"}}"#,
            "\n",
        );
        let (messages, _) = parse_claude(&tail(source));
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].markdown, "please **fix** it");
        assert!(messages[1].markdown.contains("fn main"));
        assert!(!messages[1].markdown.contains("private"));
        assert_eq!(messages[2].kind, "tool");
        assert_eq!(messages[2].tool_name.as_deref(), Some("Bash"));
        assert!(
            messages[2]
                .tool_input
                .as_deref()
                .unwrap()
                .contains("[redacted]")
        );
        assert!(
            !messages[2]
                .tool_input
                .as_deref()
                .unwrap()
                .contains("hide me")
        );
        assert_eq!(messages[2].tool_output.as_deref(), Some("ok"));
    }

    #[test]
    fn codex_keeps_messages_and_bounded_tool_records_without_reasoning() {
        let source = concat!(
            r#"{"timestamp":"one","type":"response_item","payload":{"type":"message","id":"u1","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
            "\n",
            r##"{"timestamp":"one","type":"response_item","payload":{"type":"message","id":"meta","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\n\n<INSTRUCTIONS>hidden</INSTRUCTIONS>"}]}}"##,
            "\n",
            r#"{"timestamp":"two","type":"response_item","payload":{"type":"reasoning","summary":[{"text":"private"}]}}"#,
            "\n",
            r#"{"timestamp":"three","type":"response_item","payload":{"type":"message","id":"a1","role":"assistant","content":[{"type":"output_text","text":"Hi [there](https://example.com)."}]}}"#,
            "\n",
            r#"{"timestamp":"four","type":"response_item","payload":{"type":"function_call","id":"f1","name":"exec_command","arguments":"{\"command\":\"echo ok\",\"password\":\"hide me\"}","call_id":"call-1"}}"#,
            "\n",
            r#"{"timestamp":"five","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}"#,
            "\n",
        );
        let (messages, _) = parse_codex(&tail(source));
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].markdown, "hello");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].kind, "tool");
        assert_eq!(messages[2].tool_name.as_deref(), Some("exec_command"));
        assert!(
            messages[2]
                .tool_input
                .as_deref()
                .unwrap()
                .contains("[redacted]")
        );
        assert_eq!(messages[2].tool_output.as_deref(), Some("ok"));
        assert!(
            !messages
                .iter()
                .any(|message| message.markdown.contains("private"))
        );
        assert!(
            !messages[2]
                .tool_input
                .as_deref()
                .unwrap()
                .contains("hide me")
        );
    }

    #[test]
    fn tool_strings_redact_embedded_headers_assignments_json_and_private_keys() {
        let input = serde_json::json!({
            "command": "curl -H 'X-Api-Key: sk-live' https://example.com",
            "environment": "GITHUB_TOKEN=ghp_private",
            "nested": ["Authorization: Bearer jwt-private", "safe value"]
        });
        let redacted = serde_json::to_string(&redact_json_at_depth(&input, 0)).unwrap();
        for secret in ["sk-live", "ghp_private", "jwt-private"] {
            assert!(!redacted.contains(secret));
        }
        assert!(redacted.contains("safe value"));

        let json_output = tool_output_text(Some(&Value::String(
            r#"{"password":"json-secret","safe":"visible"}"#.to_owned(),
        )))
        .unwrap();
        assert!(!json_output.contains("json-secret"));
        assert!(json_output.contains("visible"));

        let pem = sanitize_tool_text(
            "before\n-----BEGIN PRIVATE KEY-----\nbase64-private\n-----END PRIVATE KEY-----\nafter",
        )
        .unwrap();
        assert!(!pem.contains("base64-private"));
        assert!(pem.contains("[redacted private key]"));
        assert!(pem.contains("before"));
        assert!(pem.contains("after"));
    }

    #[test]
    fn nested_json_strings_and_claude_text_results_are_redacted_recursively() {
        let input = serde_json::json!({
            "payload": r#"{"credential":"nested-secret","safe":"visible"}"#
        });
        let rendered = tool_input_text(Some(&input)).unwrap();
        assert!(!rendered.contains("nested-secret"));
        assert!(rendered.contains("visible"));

        let blocks = serde_json::json!([{
            "type": "text",
            "text": r#"{"secret":"block-secret","safe":"result"}"#
        }]);
        let rendered = tool_output_text(Some(&blocks)).unwrap();
        assert!(!rendered.contains("block-secret"));
        assert!(rendered.contains("result"));

        let mut nested = r#"{"credential":"deep-secret"}"#.to_owned();
        for _ in 0..=MAX_NESTED_JSON_DEPTH {
            nested = serde_json::json!({ "payload": nested }).to_string();
        }
        let rendered = tool_input_text(Some(&Value::String(nested))).unwrap();
        assert!(!rendered.contains("deep-secret"));
        assert!(rendered.contains("redacted deeply nested JSON"));
    }

    #[test]
    fn pathological_tool_json_is_serialized_directly_into_a_bounded_writer() {
        let large = serde_json::json!({
            "rows": vec!["0123456789abcdef0123456789abcdef"; 20_000]
        });
        let rendered = tool_input_text(Some(&large)).unwrap();
        assert!(rendered.len() <= MAX_MESSAGE_BYTES);
        assert!(rendered.contains("tool JSON truncated by atmux"));

        let mut deep = serde_json::json!({ "credential": "deep-secret" });
        for _ in 0..96 {
            deep = serde_json::json!({ "safe": deep });
        }
        let rendered = tool_output_text(Some(&deep)).unwrap();
        assert!(rendered.len() <= MAX_MESSAGE_BYTES);
        assert!(!rendered.contains("deep-secret"));
    }

    #[test]
    fn pathological_tool_result_arrays_are_aggregate_bounded() {
        let mut item = serde_json::json!({ "safe": "x".repeat(2_048) });
        for _ in 0..80 {
            item = serde_json::json!({ "nested": item });
        }
        let items = Value::Array(vec![item; 64]);
        let rendered = tool_output_text(Some(&items)).unwrap();
        assert!(rendered.len() <= MAX_MESSAGE_BYTES);
        assert!(rendered.contains("tool output truncated by atmux"));
    }

    #[test]
    fn oversized_claude_ids_are_rejected_before_deriving_many_block_ids() {
        let oversized_id = "u".repeat(MAX_MESSAGE_BYTES);
        assert!(derived_id(Some(&oversized_id), "text", 0).is_none());

        let blocks = (0..MAX_PARSE_MESSAGES + 32)
            .map(|index| serde_json::json!({ "type": "text", "text": format!("block {index}") }))
            .collect::<Vec<_>>();
        let source = serde_json::json!({
            "uuid": oversized_id,
            "timestamp": "now",
            "message": { "role": "assistant", "content": blocks }
        })
        .to_string();
        let (messages, _) = parse_claude(&tail(&source));
        assert!(!messages.is_empty());
        assert!(messages.len() <= MAX_PARSE_MESSAGES);
        assert!(messages.iter().all(|message| {
            message.id.len() <= MAX_ITEM_ID_BYTES && message.id.starts_with("message-")
        }));
    }

    #[test]
    fn older_codex_event_messages_are_supported_without_tool_records() {
        let source = concat!(
            r#"{"timestamp":"one","type":"event_msg","payload":{"type":"user_message","message":"old prompt","images":[]}}"#,
            "\n",
            r#"{"timestamp":"two","type":"event_msg","payload":{"type":"agent_message","message":"old answer"}}"#,
            "\n",
            r#"{"timestamp":"three","type":"event_msg","payload":{"type":"exec_command_end","output":"private tool output"}}"#,
            "\n",
        );
        let (messages, _) = parse_codex(&tail(source));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].markdown, "old prompt");
        assert_eq!(messages[1].markdown, "old answer");
    }

    #[test]
    fn claude_session_switch_replaces_the_old_log_for_the_same_pid() {
        let home = fixture_root("claude-switch");
        let cwd = home.join("work");
        let root = home.join(".claude");
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let first_id = "11111111-1111-1111-1111-111111111111";
        let second_id = "22222222-2222-2222-2222-222222222222";
        let projects = root.join("projects").join(encode_claude_project(&cwd));
        fs::create_dir_all(&projects).unwrap();
        let first = projects.join(format!("{first_id}.jsonl"));
        let second = projects.join(format!("{second_id}.jsonl"));
        fs::write(&first, "first\n").unwrap();
        fs::write(&second, "second\n").unwrap();
        let selected = session(AgentKind::Claude, cwd.clone(), 42, 1_000);
        let metadata = root.join("sessions/42.json");
        fs::write(
            &metadata,
            format!(
                r#"{{"pid":42,"cwd":"{}","startedAt":1000,"sessionId":"{first_id}"}}"#,
                cwd.display()
            ),
        )
        .unwrap();
        assert_eq!(locate_claude(&selected, &home), Some(first.clone()));
        assert_eq!(
            claude_resume_target_in_home(&selected, &home),
            Some(ClaudeResumeTarget {
                config_dir: root.clone(),
                session_id: first_id.to_owned(),
                log_path: first.clone(),
            })
        );
        fs::write(
            metadata,
            format!(
                r#"{{"pid":42,"cwd":"{}","startedAt":1000,"sessionId":"{second_id}"}}"#,
                cwd.display()
            ),
        )
        .unwrap();
        assert_eq!(locate_claude(&selected, &home), Some(second));
        assert_eq!(
            claude_resume_target_in_home(&selected, &home).map(|target| target.session_id),
            Some(second_id.to_owned())
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn claude_log_created_after_process_metadata_is_mapped_on_a_later_read() {
        let home = fixture_root("claude-delayed-log");
        let cwd = home.join("work");
        let root = home.join(".claude-max");
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let selected = session(AgentKind::Claude, cwd.clone(), 42, 1_000);
        let session_id = "11111111-1111-1111-1111-111111111111";
        fs::write(
            root.join("sessions/42.json"),
            format!(
                r#"{{"pid":42,"cwd":"{}","startedAt":1000,"sessionId":"{session_id}"}}"#,
                cwd.display()
            ),
        )
        .unwrap();

        // Claude can publish process metadata before its first JSONL record.
        // An unavailable read is retried by the browser and must not cache a
        // guessed same-directory conversation.
        assert_eq!(locate_claude(&selected, &home), None);

        let projects = root.join("projects").join(encode_claude_project(&cwd));
        fs::create_dir_all(&projects).unwrap();
        let log = projects.join(format!("{session_id}.jsonl"));
        fs::write(&log, "message\n").unwrap();
        assert_eq!(locate_claude(&selected, &home), Some(log));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn claude_root_scan_finds_an_unlabeled_profile_and_refuses_ambiguity() {
        let home = fixture_root("claude-profile-scan");
        let cwd = home.join("work");
        fs::create_dir_all(&cwd).unwrap();
        let selected = session(AgentKind::Claude, cwd.clone(), 42, 1_000);
        let session_id = "11111111-1111-1111-1111-111111111111";

        let create_log = |root: &Path| {
            fs::create_dir_all(root.join("sessions")).unwrap();
            let projects = root.join("projects").join(encode_claude_project(&cwd));
            fs::create_dir_all(&projects).unwrap();
            let log = projects.join(format!("{session_id}.jsonl"));
            fs::write(&log, "message\n").unwrap();
            fs::write(
                root.join("sessions/42.json"),
                format!(
                    r#"{{"pid":42,"cwd":"{}","startedAt":1000,"sessionId":"{session_id}"}}"#,
                    cwd.display()
                ),
            )
            .unwrap();
            log
        };

        let profile_log = create_log(&home.join(".claude-max"));
        assert_eq!(locate_claude(&selected, &home), Some(profile_log));
        create_log(&home.join(".claude"));
        assert_eq!(locate_claude(&selected, &home), None);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn codex_open_rollout_switch_never_returns_the_old_thread() {
        let home = fixture_root("codex-switch");
        let root = home.join(".codex");
        let cwd = home.join("work");
        let sessions = root.join("sessions/2026/08/07");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let first_id = "11111111-1111-1111-1111-111111111111";
        let second_id = "22222222-2222-2222-2222-222222222222";
        let first = sessions.join(format!("rollout-one-{first_id}.jsonl"));
        let second = sessions.join(format!("rollout-two-{second_id}.jsonl"));
        for (path, id) in [(&first, first_id), (&second, second_id)] {
            fs::write(
                path,
                format!(
                    r#"{{"type":"session_meta","payload":{{"id":"{id}","source":"cli","thread_source":"user","cwd":"{}"}}}}"#,
                    cwd.display()
                ),
            )
            .unwrap();
        }
        assert_eq!(
            select_codex_rollout([first.clone()], &root, &cwd),
            Some(first.clone())
        );
        assert_eq!(
            select_codex_rollout([second.clone()], &root, &cwd),
            Some(second.clone())
        );
        assert_eq!(select_codex_rollout([first, second], &root, &cwd), None);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn codex_subagent_parent_metadata_never_makes_the_child_match() {
        let home = fixture_root("codex-subagent-parent");
        let root = home.join(".codex");
        let cwd = home.join("work");
        let sessions = root.join("sessions/2026/08/09");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let parent_id = "11111111-1111-1111-1111-111111111111";
        let child_id = "22222222-2222-2222-2222-222222222222";
        let parent = sessions.join(format!("rollout-parent-{parent_id}.jsonl"));
        let child = sessions.join(format!("rollout-child-{child_id}.jsonl"));
        let parent_meta = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{parent_id}","source":"cli","thread_source":"user","cwd":"{}"}}}}"#,
            cwd.display()
        );
        let child_meta = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{child_id}","source":{{"subagent":{{}}}},"thread_source":"subagent","cwd":"{}"}}}}"#,
            cwd.display()
        );
        fs::write(&parent, format!("{parent_meta}\n")).unwrap();
        fs::write(&child, format!("{child_meta}\n{parent_meta}\n")).unwrap();

        assert_eq!(
            select_codex_rollout([parent.clone(), child], &root, &cwd),
            Some(parent)
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_log_growing_after_metadata_is_sampled_stays_read_bounded() {
        let source = std::io::Cursor::new(vec![b'x'; 4 * 1024 * 1024 + 4_096]);
        let bounded = read_bounded(source, 0).unwrap();
        assert_eq!(
            bounded.bytes.len(),
            usize::try_from(MAX_LOG_TAIL_BYTES).unwrap()
        );
        assert!(bounded.read_capped);
    }

    #[test]
    fn claude_metadata_and_root_enumeration_fail_closed_at_their_caps() {
        let home = fixture_root("metadata-caps");
        let oversized = home.join("metadata.json");
        fs::write(
            &oversized,
            vec![b' '; usize::try_from(MAX_CLAUDE_METADATA_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(read_bounded_json(&oversized, MAX_CLAUDE_METADATA_BYTES).is_none());

        for index in 0..=MAX_CLAUDE_ROOTS {
            fs::create_dir(home.join(format!(".claude-profile{index}"))).unwrap();
        }
        assert!(claude_roots("claude", &home).is_none());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn claude_root_accepts_only_conventional_direct_children_of_home() {
        let home = Path::new("/Users/ryan");
        assert_eq!(
            claude_root(
                "CLAUDE_CONFIG_DIR=/Users/ryan/.claude-max · /opt/homebrew/bin/claude",
                home,
            ),
            PathBuf::from("/Users/ryan/.claude-max")
        );
        assert_eq!(
            claude_root(
                "CLAUDE_CONFIG_DIR=/tmp/attacker · /opt/homebrew/bin/claude",
                home,
            ),
            PathBuf::from("/Users/ryan/.claude")
        );
    }

    #[test]
    fn bounds_total_transcript_from_the_front() {
        let messages = (0..300)
            .map(|index| TranscriptMessage {
                id: index.to_string(),
                role: "assistant".to_owned(),
                kind: default_message_kind(),
                markdown: "x".repeat(4_000),
                tool_name: None,
                tool_input: None,
                tool_output: None,
                timestamp: None,
            })
            .collect();
        let (bounded, truncated) = bound_messages(messages);
        assert!(truncated);
        assert!(bounded.len() <= MAX_MESSAGES);
        assert!(
            bounded
                .iter()
                .map(|message| message.markdown.len())
                .sum::<usize>()
                <= MAX_TRANSCRIPT_BYTES
        );
        assert_eq!(bounded.last().unwrap().id, "299");
    }

    #[test]
    fn transcript_budget_counts_serialized_ids_timestamps_and_escaping() {
        let messages = (0..300)
            .map(|index| TranscriptMessage {
                id: format!("{index}-{}", "id".repeat(900)),
                role: "assistant".to_owned(),
                kind: default_message_kind(),
                markdown: "\u{0000}".repeat(2_000),
                tool_name: None,
                tool_input: None,
                tool_output: None,
                timestamp: Some("t".repeat(1_000)),
            })
            .collect();
        let (bounded, truncated) = bound_messages(messages);
        assert!(truncated);
        assert!(serde_json::to_vec(&bounded).unwrap().len() <= MAX_TRANSCRIPT_BYTES);

        let mut parsed = Vec::new();
        for index in 0..10_000 {
            push_message(
                &mut parsed,
                "assistant",
                "ok".to_owned(),
                Some(&format!("id-{index}-{}", "x".repeat(MAX_ITEM_ID_BYTES + 1))),
                Some(&"t".repeat(MAX_TIMESTAMP_BYTES + 1)),
            );
        }
        assert!(parsed.len() <= MAX_PARSE_MESSAGES);
        assert!(
            parsed
                .iter()
                .all(|message| message.id.len() <= MAX_ITEM_ID_BYTES)
        );
        assert!(parsed.iter().all(|message| {
            message
                .timestamp
                .as_deref()
                .is_none_or(|timestamp| timestamp.len() <= MAX_TIMESTAMP_BYTES)
        }));
    }
}
