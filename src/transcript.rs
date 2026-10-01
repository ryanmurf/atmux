//! Bounded, read-only views of the conversation logs written by agent CLIs.
//!
//! The browser never chooses a filesystem path.  atmux derives candidates from
//! the selected tmux pane's agent kind, working directory, and non-sensitive
//! launch label. It returns human/assistant messages plus bounded tool calls
//! and results; system prompts, environment records, and reasoning stay
//! server-side.

use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
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
const TASK_NOTIFICATION_TAG: &str = "<task-notification>";
const MAX_NESTED_JSON_DEPTH: usize = 8;
const MAX_CLAUDE_ROOTS: usize = 64;
// Claude writes its PID metadata after CLI initialization and authentication;
// that can lag the OS process start noticeably on macOS and networked auth.
// The exact live PID + cwd check remains the primary identity boundary.
const MAX_PROCESS_LOG_START_SKEW_MS: u64 = 120_000;
const MAX_PROCESS_ARGV_BYTES: u64 = 64 * 1024;
const MAX_CODEX_DAY_DIRECTORIES: u64 = 40;
const MAX_CODEX_DAY_ENTRIES: usize = 20_000;
const MAX_COMPACTION_SUMMARY_BYTES: usize = 64 * 1024;

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
    /// Names the subagent that produced a `subagent` entry when the native log
    /// carries one. Absent for operator and main-agent turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Native per-request usage for the entry, so the browser can total a
    /// collapsed run of tool calls without re-reading the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Present only on a `compaction` entry: what the CLI recorded about the
    /// compaction. The entry's markdown carries the summary text, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionDetail>,
}

/// Native metadata of one conversation compaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompactionDetail {
    /// `manual` or `auto` when the CLI recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_tokens: Option<u64>,
}

/// Optional per-entry attribution and usage. Grouping them keeps the record
/// builders at their existing arity for the harnesses that publish neither.
#[derive(Clone, Copy, Default)]
struct EntryMeta<'a> {
    agent_name: Option<&'a str>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl EntryMeta<'_> {
    fn usage(usage: Option<(u64, u64)>) -> Self {
        Self {
            agent_name: None,
            input_tokens: usage.map(|(input, _)| input),
            output_tokens: usage.map(|(_, output)| output),
        }
    }
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
    /// A short owner-written explanation of an unusual mapping state, such as
    /// a CLI that has not finished starting. Never a path or session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
            note: None,
        }
    }

    fn unavailable_with_note(source: &str, note: &str) -> Self {
        Self {
            note: Some(note.to_owned()),
            ..Self::unavailable(source)
        }
    }
}

const NOTE_CLAUDE_STARTING: &str = "Claude has not finished starting, so its conversation is not published yet. It may be waiting at a startup prompt; check Raw pane.";
const NOTE_CLAUDE_RESUMING: &str = "Claude has not finished starting (it may be waiting at a startup prompt in Raw pane). Showing the conversation it was launched to resume.";
const NOTE_CLAUDE_EMPTY: &str = "This Claude session has no messages yet.";
const NOTE_CODEX_RESUMED: &str = "Codex has not written to this conversation since it was resumed. Showing the conversation it was launched to resume.";

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
    let home = env::var_os("HOME").map(PathBuf::from);
    let (path, note) = match (session.agent, home.as_deref()) {
        (AgentKind::Other, _) | (_, None) => return Ok(Transcript::unavailable(source)),
        (AgentKind::Claude, Some(home)) => match claude_lookup_in_home(session, home) {
            ClaudeLookup::Mapped(target) => (target.log_path, None),
            ClaudeLookup::AwaitingFirstMessage(log_path) => {
                return Ok(empty_transcript(
                    source,
                    &log_path,
                    known_hash,
                    NOTE_CLAUDE_EMPTY,
                ));
            }
            ClaudeLookup::Starting {
                resuming: Some(log_path),
            } => (log_path, Some(NOTE_CLAUDE_RESUMING)),
            ClaudeLookup::Starting { resuming: None } => {
                return Ok(Transcript::unavailable_with_note(
                    source,
                    NOTE_CLAUDE_STARTING,
                ));
            }
            ClaudeLookup::Unmapped => return Ok(Transcript::unavailable(source)),
        },
        (AgentKind::Codex, Some(home)) => {
            match codex_lookup(session, &codex_root(&session.launch_command, home)) {
                CodexLookup::Open(path) => (path, None),
                CodexLookup::Resumed(path) => (path, Some(NOTE_CODEX_RESUMED)),
                CodexLookup::Unmapped => return Ok(Transcript::unavailable(source)),
            }
        }
    };
    let note = note.map(str::to_owned);
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
            note,
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
        note,
    })
}

/// A mapped session whose native log does not exist yet: available, empty,
/// and stable until the CLI writes its first record.
fn empty_transcript(
    source: &str,
    expected_log: &Path,
    known_hash: Option<&str>,
    note: &str,
) -> Transcript {
    let content_hash = transcript_hash(source, expected_log, &[]);
    let changed = known_hash != Some(content_hash.as_str());
    Transcript {
        available: true,
        source: source.to_owned(),
        content_hash,
        changed,
        truncated: false,
        messages: changed.then(Vec::new),
        note: Some(note.to_owned()),
    }
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
    match claude_lookup_in_home(session, home) {
        ClaudeLookup::Mapped(target) => Some(target),
        _ => None,
    }
}

/// How one live Claude pane relates to the native logs on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ClaudeLookup {
    /// Exactly one config root has live metadata for this process, and it
    /// names an existing log.
    Mapped(ClaudeResumeTarget),
    /// The live metadata is exact, but Claude has not written the log yet.
    AwaitingFirstMessage(PathBuf),
    /// No config root has metadata for this process yet. Claude publishes it
    /// only once startup finishes, which a startup prompt (development
    /// channels, trust) can delay indefinitely. `resuming` is the one log
    /// the process was explicitly launched to resume, for display only.
    Starting { resuming: Option<PathBuf> },
    /// Missing identity, rejected or ambiguous metadata.
    Unmapped,
}

enum ClaudeMetadata {
    Absent,
    Rejected,
    Matched {
        target: ClaudeResumeTarget,
        log_exists: bool,
    },
}

fn claude_lookup_in_home(session: &Session, home: &Path) -> ClaudeLookup {
    if session.agent != AgentKind::Claude {
        return ClaudeLookup::Unmapped;
    }
    let Some(pid) = session.agent_pid.filter(|pid| *pid > 0) else {
        return ClaudeLookup::Unmapped;
    };
    let Some(roots) = claude_roots(&session.launch_command, home) else {
        return ClaudeLookup::Unmapped;
    };
    let mut any_metadata = false;
    let mut matching = Vec::new();
    for root in &roots {
        match claude_metadata_in_root(session, pid, root) {
            ClaudeMetadata::Absent => {}
            ClaudeMetadata::Rejected => any_metadata = true,
            ClaudeMetadata::Matched { target, log_exists } => {
                any_metadata = true;
                matching.push((target, log_exists));
            }
        }
    }
    if matching.len() == 1 {
        let (target, log_exists) = matching.remove(0);
        return if log_exists {
            ClaudeLookup::Mapped(target)
        } else {
            ClaudeLookup::AwaitingFirstMessage(target.log_path)
        };
    }
    if any_metadata {
        // Rejected or ambiguous live metadata: never fall back to argv, which
        // cannot see a later /clear or /resume inside the process.
        return ClaudeLookup::Unmapped;
    }
    ClaudeLookup::Starting {
        resuming: claude_argv_resume_log(session, pid, &roots),
    }
}

fn claude_metadata_in_root(session: &Session, pid: u32, root: &Path) -> ClaudeMetadata {
    let metadata_path = root.join("sessions").join(format!("{pid}.json"));
    match fs::symlink_metadata(&metadata_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ClaudeMetadata::Absent;
        }
        Err(_) => return ClaudeMetadata::Rejected,
        Ok(_) => {}
    }
    if !regular_file_within(&metadata_path, root) {
        return ClaudeMetadata::Rejected;
    }
    let Some(metadata) = read_bounded_json(&metadata_path, MAX_CLAUDE_METADATA_BYTES) else {
        return ClaudeMetadata::Rejected;
    };
    if metadata.get("pid").and_then(Value::as_u64) != Some(u64::from(pid))
        || metadata
            .get("cwd")
            .and_then(Value::as_str)
            .is_none_or(|cwd| Path::new(cwd) != session.path)
        || !claude_metadata_pane_matches(&metadata, &session.pane_id)
        || !claude_pid_domain_matches(&metadata)
        || !claude_metadata_process_matches(session, pid, &metadata)
    {
        return ClaudeMetadata::Rejected;
    }
    let Some(session_id) = metadata
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| valid_session_id(value))
    else {
        return ClaudeMetadata::Rejected;
    };
    let log_path = claude_log_path(root, &session.path, session_id);
    let log_exists = regular_file_within(&log_path, root);
    ClaudeMetadata::Matched {
        target: ClaudeResumeTarget {
            config_dir: root.to_owned(),
            session_id: session_id.to_owned(),
            log_path,
        },
        log_exists,
    }
}

fn claude_log_path(root: &Path, cwd: &Path, session_id: &str) -> PathBuf {
    root.join("projects")
        .join(encode_claude_project(cwd))
        .join(format!("{session_id}.jsonl"))
}

/// Claude records its session start (`startedAt`) when startup finishes, not
/// when the OS started the process. A startup prompt answered minutes later
/// therefore fails the start-time window even though the metadata is exact.
/// Current Claude versions also record the OS process start (`procStart`):
/// Linux clock ticks since boot, or the UTC `ps -o lstart` text on macOS.
/// That value is exact, so it decides whenever the looser window does not.
fn claude_metadata_process_matches(session: &Session, pid: u32, metadata: &Value) -> bool {
    if starts_close_enough(
        session.agent_started_ms,
        metadata.get("startedAt").and_then(Value::as_u64),
    ) {
        return true;
    }
    let recorded = match metadata.get("procStart") {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        _ => return false,
    };
    crate::control::native_process_start_stamp(pid)
        .is_some_and(|stamp| process_start_stamp_matches(&stamp, &recorded))
}

fn process_start_stamp_matches(stamp: &str, recorded: &str) -> bool {
    let normalize = |value: &str| value.split_whitespace().collect::<Vec<_>>().join(" ");
    let recorded = normalize(recorded);
    if recorded.is_empty() {
        return false;
    }
    if let Some(ticks) = stamp.strip_prefix("linux:") {
        return recorded == ticks;
    }
    stamp
        .strip_prefix("macos:")
        .is_some_and(|text| recorded == normalize(text))
}

/// Current Claude versions record the tmux binding (`session:@window.%pane`)
/// they started in. A different pane means the metadata is not this pane's.
fn claude_metadata_pane_matches(metadata: &Value, pane_id: &str) -> bool {
    let Some(binding) = metadata.get("tmux").and_then(Value::as_str) else {
        return true;
    };
    match binding.rsplit_once('.') {
        Some((_, pane)) if valid_pane_id(pane) => pane == pane_id,
        _ => true,
    }
}

fn valid_pane_id(value: &str) -> bool {
    value.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// Current Claude versions name the PID namespace a PID belongs to. On Linux
/// it is `linux:<machine-id>:pid:[<namespace inode>]`, so a PID recorded on
/// another machine sharing this home directory, or inside another PID
/// namespace, can never match. A part this process cannot read is skipped.
#[cfg(target_os = "linux")]
fn claude_pid_domain_matches(metadata: &Value) -> bool {
    let Some(domain) = metadata.get("pidDomain").and_then(Value::as_str) else {
        return true;
    };
    let local_machine = ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|path| fs::read_to_string(path).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let local_namespace = fs::read_link("/proc/self/ns/pid")
        .ok()
        .and_then(|link| link.to_str().map(str::to_owned));
    linux_pid_domain_matches(domain, local_machine.as_deref(), local_namespace.as_deref())
}

#[cfg(any(target_os = "linux", test))]
fn linux_pid_domain_matches(domain: &str, machine: Option<&str>, namespace: Option<&str>) -> bool {
    let Some(rest) = domain.strip_prefix("linux:") else {
        return false;
    };
    let (recorded_machine, recorded_namespace) = rest
        .split_once(':')
        .map_or((rest, None), |(machine, namespace)| {
            (machine, Some(namespace))
        });
    let machine_matches =
        machine.is_none_or(|machine| recorded_machine.eq_ignore_ascii_case(machine));
    let namespace_matches = match (recorded_namespace, namespace) {
        (Some(recorded), Some(local)) => recorded == local,
        _ => true,
    };
    machine_matches && namespace_matches
}

#[cfg(target_os = "macos")]
fn claude_pid_domain_matches(metadata: &Value) -> bool {
    metadata
        .get("pidDomain")
        .and_then(Value::as_str)
        .is_none_or(|domain| domain == "darwin")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn claude_pid_domain_matches(_metadata: &Value) -> bool {
    true
}

/// The one existing log a still-starting Claude process was explicitly
/// launched to resume. Display-only: callers that mutate (restart,
/// maintenance relaunch, auto-compact) never use this path.
fn claude_argv_resume_log(session: &Session, pid: u32, roots: &[PathBuf]) -> Option<PathBuf> {
    claude_resume_log_for_argv(&process_argv(pid)?, session, roots)
}

fn claude_resume_log_for_argv(
    argv: &[String],
    session: &Session,
    roots: &[PathBuf],
) -> Option<PathBuf> {
    let session_id = claude_argv_resume_id(argv)?;
    let matching = roots
        .iter()
        .map(|root| (root, claude_log_path(root, &session.path, &session_id)))
        .filter(|(root, log)| regular_file_within(log, root))
        .map(|(_, log)| log)
        .collect::<Vec<_>>();
    (matching.len() == 1).then(|| matching[0].clone())
}

/// The session id named by `--resume <id>`, `--resume=<id>`, `-r <id>` or
/// `--session-id <id>`. A picker (`--resume` without an id), `--continue`,
/// `--fork-session`, or two different ids name no single existing log.
fn claude_argv_resume_id(argv: &[String]) -> Option<String> {
    let mut found: Option<String> = None;
    let mut record = |value: &str| -> Option<()> {
        if !valid_session_id(value) || found.as_deref().is_some_and(|seen| seen != value) {
            return None;
        }
        found = Some(value.to_owned());
        Some(())
    };
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--" => break,
            "--fork-session" | "--continue" | "-c" => return None,
            "--resume" | "-r" | "--session-id" => record(arguments.next()?)?,
            other => {
                if let Some(value) = other
                    .strip_prefix("--resume=")
                    .or_else(|| other.strip_prefix("--session-id="))
                {
                    record(value)?;
                }
            }
        }
    }
    found
}

/// The live process's argument vector, or `None` when it cannot be read.
#[cfg(target_os = "linux")]
fn process_argv(pid: u32) -> Option<Vec<String>> {
    let mut bytes = Vec::new();
    fs::File::open(format!("/proc/{pid}/cmdline"))
        .ok()?
        .take(MAX_PROCESS_ARGV_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let argv = bytes
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8(argument.to_vec()).ok())
        .collect::<Option<Vec<_>>>()?;
    (!argv.is_empty()).then_some(argv)
}

/// macOS exposes argv only through `ps`, joined with spaces. That is enough
/// here: the callers accept only session-id tokens, which contain no spaces.
#[cfg(target_os = "macos")]
fn process_argv(pid: u32) -> Option<Vec<String>> {
    let output = std::process::Command::new("/bin/ps")
        .env("LC_ALL", "C")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > MAX_PROCESS_ARGV_BYTES as usize {
        return None;
    }
    let argv = String::from_utf8(output.stdout)
        .ok()?
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    (!argv.is_empty()).then_some(argv)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_argv(_pid: u32) -> Option<Vec<String>> {
    None
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

/// How one live Codex pane relates to its rollouts, for display.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CodexLookup {
    /// The process holds exactly one matching rollout open.
    Open(PathBuf),
    /// The process holds none open: it was launched as `codex resume <id>`
    /// and has not written since. Codex opens the resumed rollout lazily.
    Resumed(PathBuf),
    Unmapped,
}

fn codex_lookup(session: &Session, root: &Path) -> CodexLookup {
    let Some(pid) = session.agent_pid.filter(|pid| *pid > 0) else {
        return CodexLookup::Unmapped;
    };
    let Ok(paths) = process_open_paths(pid) else {
        return CodexLookup::Unmapped;
    };
    let open = open_codex_rollouts(paths, root, &session.path);
    match open.len() {
        1 => CodexLookup::Open(open.into_iter().next().unwrap_or_default()),
        0 => codex_resumed_rollout(session, pid, root)
            .map_or(CodexLookup::Unmapped, CodexLookup::Resumed),
        _ => CodexLookup::Unmapped,
    }
}

/// The rollout a Codex process was launched to resume, when it provably has
/// not moved on. Any other same-directory user thread created after the
/// process started (a `/new`, or a sibling process) makes it ambiguous.
fn codex_resumed_rollout(session: &Session, pid: u32, root: &Path) -> Option<PathBuf> {
    let now_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()?;
    codex_resumed_rollout_for_argv(&process_argv(pid)?, session, root, now_ms)
}

fn codex_resumed_rollout_for_argv(
    argv: &[String],
    session: &Session,
    root: &Path,
    now_ms: u64,
) -> Option<PathBuf> {
    let session_id = codex_argv_resume_id(argv)?;
    let created_ms = uuid_v7_millis(&session_id)?;
    let started_ms = session.agent_started_ms?;
    let sessions = root.join("sessions");
    let suffix = format!("-{session_id}.jsonl");
    let target = codex_day_directories(&sessions, created_ms, created_ms)?
        .into_iter()
        .flat_map(|directory| bounded_directory_files(&directory).unwrap_or_default())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
                && regular_file_within(path, root)
                && codex_log_matches(path, &session.path)
        })?;
    for directory in codex_day_directories(&sessions, started_ms, now_ms)? {
        for path in bounded_directory_files(&directory)? {
            let Some(other) = rollout_session_id(&path) else {
                continue;
            };
            if other == session_id {
                continue;
            }
            // Thread ids are `UUIDv7`: their creation time is exact. A thread
            // whose time cannot be read is not provably older, so fail closed.
            let newer = uuid_v7_millis(other).is_none_or(|created| created >= started_ms);
            if newer && regular_file_within(&path, root) && codex_log_matches(&path, &session.path)
            {
                return None;
            }
        }
    }
    Some(target)
}

/// The thread id named by `codex [options] resume [options] <id>`.
fn codex_argv_resume_id(argv: &[String]) -> Option<String> {
    let position = argv
        .iter()
        .skip(1)
        .position(|argument| argument == "resume")?
        + 1;
    let mut ids = argv[position + 1..]
        .iter()
        .take_while(|argument| argument.as_str() != "--")
        .filter(|argument| valid_session_id(argument));
    let id = ids.next()?.clone();
    if ids.next().is_some()
        || argv[position + 1..]
            .iter()
            .any(|argument| argument == "--last")
    {
        return None;
    }
    Some(id)
}

fn rollout_session_id(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    valid_session_id(id).then_some(id)
}

/// Milliseconds since the epoch encoded in a `UUIDv7`, or `None` for any other
/// version.
fn uuid_v7_millis(id: &str) -> Option<u64> {
    if !valid_session_id(id) || id.as_bytes().get(14) != Some(&b'7') {
        return None;
    }
    let hex = format!("{}{}", id.get(0..8)?, id.get(9..13)?);
    u64::from_str_radix(&hex, 16).ok()
}

/// Codex files rollouts under `sessions/YYYY/MM/DD` in local time. Cover the
/// UTC days spanned by `[from_ms, to_ms]` plus one day either side for any
/// UTC offset, and refuse an unbounded span.
fn codex_day_directories(sessions: &Path, from_ms: u64, to_ms: u64) -> Option<Vec<PathBuf>> {
    const DAY_MS: u64 = 86_400_000;
    let first = (from_ms / DAY_MS).checked_sub(1)?;
    let last = to_ms / DAY_MS + 1;
    if last < first || last - first > MAX_CODEX_DAY_DIRECTORIES {
        return None;
    }
    Some(
        (first..=last)
            .map(|day| {
                let (year, month, day) = civil_from_days(day);
                sessions
                    .join(format!("{year:04}"))
                    .join(format!("{month:02}"))
                    .join(format!("{day:02}"))
            })
            .filter(|directory| regular_directory(directory))
            .collect(),
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// Regular files directly inside `directory`, or `None` past the entry cap.
fn bounded_directory_files(directory: &Path) -> Option<Vec<PathBuf>> {
    let mut files = Vec::new();
    for (index, entry) in fs::read_dir(directory).ok()?.flatten().enumerate() {
        if index >= MAX_CODEX_DAY_ENTRIES {
            return None;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_file()) {
            files.push(entry.path());
        }
    }
    Some(files)
}

fn select_codex_rollout(
    paths: impl IntoIterator<Item = PathBuf>,
    root: &Path,
    cwd: &Path,
) -> Option<PathBuf> {
    let matching = open_codex_rollouts(paths, root, cwd);
    if matching.len() == 1 {
        matching.into_iter().next()
    } else {
        // Showing no structured transcript is safer than showing a different
        // same-directory conversation or a concurrently open child thread.
        None
    }
}

fn open_codex_rollouts(
    paths: impl IntoIterator<Item = PathBuf>,
    root: &Path,
    cwd: &Path,
) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .filter(|path| regular_file_within(path, root))
        .filter(|path| codex_log_matches(path, cwd))
        .collect()
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

fn claude_request_usage(log: &LogTail) -> HashMap<String, (u64, u64)> {
    // Streaming content blocks can repeat the same model-request usage. Keep
    // its final counters and charge one visible entry, not every JSONL record.
    let mut request_usage: HashMap<String, (u64, u64)> = HashMap::new();
    for value in json_lines(&log.bytes) {
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let (Some(id), Some((input, output))) = (claude_request_id(&value), claude_usage(&value))
        {
            request_usage
                .entry(id.to_owned())
                .and_modify(|usage| {
                    usage.0 = usage.0.max(input);
                    usage.1 = usage.1.max(output);
                })
                .or_insert((input, output));
        }
    }
    request_usage
}

fn parse_claude(log: &LogTail) -> (Vec<TranscriptMessage>, bool) {
    let mut messages = Vec::new();
    let mut request_usage = claude_request_usage(log);
    for value in json_lines(&log.bytes) {
        // A subagent's own inner turns are intentionally skipped: the parent
        // log already carries the notification the subagent reported back.
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let role = value.pointer("/message/role").and_then(Value::as_str);
        let id = value.get("uuid").and_then(Value::as_str);
        let timestamp = value.get("timestamp").and_then(Value::as_str);
        if push_claude_compaction(&mut messages, &value, id, timestamp) {
            continue;
        }
        let Some(content) = value.pointer("/message/content") else {
            continue;
        };
        let visible = content.as_str().is_some_and(|text| !text.trim().is_empty())
            || content.as_array().is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block.get("type").and_then(Value::as_str) == Some("tool_use")
                        || (block.get("type").and_then(Value::as_str) == Some("text")
                            && block
                                .get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|text| !text.trim().is_empty()))
                })
            });
        let mut usage = if let Some(id) = claude_request_id(&value) {
            visible.then(|| request_usage.remove(id)).flatten()
        } else {
            claude_usage(&value)
        };
        match role {
            Some("user") => {
                push_claude_user(&mut messages, &value, content, id, timestamp, usage.take());
            }
            Some("assistant") => {
                if let Some(markdown) = content.as_str().map(str::to_owned) {
                    push_message(
                        &mut messages,
                        "assistant",
                        markdown,
                        id,
                        timestamp,
                        EntryMeta::usage(usage.take()),
                    );
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
                                    EntryMeta::usage(usage.take()),
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
                                EntryMeta::usage(usage.take()),
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

/// Claude Code persists subagent and tool-generated turns with the user role.
/// Attributing those to the operator makes an Agent tool's own prompts and
/// reports read as "You" in the browser.
fn push_claude_user(
    messages: &mut Vec<TranscriptMessage>,
    value: &Value,
    content: &Value,
    id: Option<&str>,
    timestamp: Option<&str>,
    usage: Option<(u64, u64)>,
) {
    let mut subagent = claude_tool_authored_user(value);
    let mut agent_name = subagent.then(|| claude_subagent_name(value)).flatten();
    if let Some(mut markdown) = claude_user_text(value).filter(|text| !text.trim().is_empty())
        && !is_injected_user_context(&markdown)
    {
        if let Some((name, body)) = claude_task_notification(&markdown) {
            subagent = true;
            agent_name = agent_name.or(Some(name));
            markdown = body;
        }
        push_message(
            messages,
            if subagent { "subagent" } else { "user" },
            markdown,
            id,
            timestamp,
            EntryMeta {
                agent_name: agent_name.as_deref(),
                ..EntryMeta::usage(usage)
            },
        );
    }
    let Some(blocks) = content.as_array() else {
        return;
    };
    for (index, block) in blocks.iter().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        attach_tool_output(
            messages,
            block.get("tool_use_id").and_then(Value::as_str),
            tool_output_text(block.get("content")),
            derived_id(id, "tool-result", index).as_deref(),
            timestamp,
        );
    }
}

/// Claude Code writes every subagent and tool-authored turn back into the log
/// with `message.role == "user"`. These markers are the harness's own way of
/// separating a typed prompt from a turn a tool produced.
fn claude_tool_authored_user(value: &Value) -> bool {
    value.get("turnCompanion").and_then(Value::as_bool) == Some(true)
        || value.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || value.get("userType").and_then(Value::as_str) == Some("agent")
        // Claude Code 2.1.261 stamps an Agent tool's completion turn with a
        // harness origin and prompt source instead of the older tool markers.
        || value.pointer("/origin/kind").and_then(Value::as_str) == Some("task-notification")
        || value.get("promptSource").and_then(Value::as_str) == Some("system")
        || ["sourceToolAssistantUUID", "sourceToolUseID", "taskId"]
            .iter()
            .any(|key| value.get(key).is_some_and(|marker| !marker.is_null()))
}

/// An Agent tool completion arrives as a user line whose whole body is a
/// `<task-notification>` envelope. Rendering that XML verbatim credits the
/// harness's bookkeeping to the operator, so the envelope is reduced to the
/// summary and result the subagent actually reported.
fn claude_task_notification(text: &str) -> Option<(String, String)> {
    let envelope = text.trim();
    if !envelope.starts_with(TASK_NOTIFICATION_TAG) {
        return None;
    }
    let summary = xml_element_text(envelope, "summary").unwrap_or_default();
    let name = quoted_segment(summary)
        .or(Some(summary).filter(|value| !value.is_empty()))
        .map_or_else(
            || "Task".to_owned(),
            |value| truncate_plain(value, MAX_TOOL_NAME_BYTES),
        );
    let body = ["summary", "result"]
        .iter()
        .filter_map(|tag| xml_element_text(envelope, tag))
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Some((
        name,
        if body.is_empty() {
            "Task notification".to_owned()
        } else {
            body
        },
    ))
}

/// Minimal single-element lookup for the harness's own flat notification
/// envelope. Nested or attributed elements are deliberately out of scope.
fn xml_element_text<'a>(source: &'a str, tag: &str) -> Option<&'a str> {
    let start = source.find(&format!("<{tag}>"))? + tag.len() + 2;
    let rest = source.get(start..)?;
    Some(rest[..rest.find(&format!("</{tag}>"))?].trim())
}

fn quoted_segment(value: &str) -> Option<&str> {
    let start = value.find('"')? + 1;
    let rest = value.get(start..)?;
    let quoted = rest[..rest.find('"')?].trim();
    (!quoted.is_empty()).then_some(quoted)
}

fn claude_subagent_name(value: &Value) -> Option<String> {
    [
        "agentName",
        "subagentName",
        "subagentType",
        "attributionAgent",
    ]
    .iter()
    .find_map(|key| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| truncate_plain(name, MAX_TOOL_NAME_BYTES))
    })
}

fn claude_request_id(value: &Value) -> Option<&str> {
    value
        .pointer("/message/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= MAX_ITEM_ID_BYTES)
}

/// Native usage for one request, including cached input.
fn claude_usage(value: &Value) -> Option<(u64, u64)> {
    let usage = value.pointer("/message/usage")?;
    let optional = |key: &str| usage.get(key).map_or(Some(0), Value::as_u64);
    let input = usage
        .get("input_tokens")?
        .as_u64()?
        .checked_add(optional("cache_creation_input_tokens")?)?
        .checked_add(optional("cache_read_input_tokens")?)?;
    Some((input, usage.get("output_tokens")?.as_u64()?))
}

fn claude_user_text(value: &Value) -> Option<String> {
    let content = value.pointer("/message/content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    content_text(Some(content), "text")
}

fn parse_codex(log: &LogTail) -> (Vec<TranscriptMessage>, bool) {
    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut pending_usage_entry = None;
    let mut last_usage_total = None;
    for value in json_lines(&log.bytes) {
        apply_codex_usage_event(
            &value,
            &mut messages,
            &mut pending_usage_entry,
            &mut last_usage_total,
        );
        if push_codex_compaction(&mut messages, &value) {
            continue;
        }
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
                if role == Some("user") {
                    pending_usage_entry = None;
                }
                push_message(
                    &mut messages,
                    role.unwrap_or_default(),
                    markdown,
                    value.pointer("/payload/id").and_then(Value::as_str),
                    timestamp,
                    EntryMeta::default(),
                );
                if role == Some("assistant") && pending_usage_entry.is_none() {
                    pending_usage_entry = messages.last().map(|message| message.id.clone());
                }
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
                    EntryMeta::default(),
                );
                if pending_usage_entry.is_none() {
                    pending_usage_entry = messages.last().map(|message| message.id.clone());
                }
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
        append_codex_event_messages(log, &mut messages);
    }
    (messages, false)
}

fn append_codex_event_messages(log: &LogTail, messages: &mut Vec<TranscriptMessage>) {
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
            messages,
            role,
            markdown,
            None,
            value.get("timestamp").and_then(Value::as_str),
            EntryMeta::default(),
        );
    }
}

fn apply_codex_usage_event(
    value: &Value,
    messages: &mut [TranscriptMessage],
    pending: &mut Option<String>,
    last_total: &mut Option<(u64, u64)>,
) {
    if value.get("type").and_then(Value::as_str) == Some("compacted") {
        *pending = None;
        *last_total = None;
    }
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    match value.pointer("/payload/type").and_then(Value::as_str) {
        Some("token_count") => {
            let total = codex_usage_pair(value.pointer("/payload/info/total_token_usage"));
            // Repeated status/rate-limit events must not charge the previous
            // request again or steal the next call's usage.
            if total.is_some() && total == *last_total {
                return;
            }
            if total.is_some() {
                *last_total = total;
            }
            if let Some(id) = pending.take()
                && let Some((input, output)) =
                    codex_usage_pair(value.pointer("/payload/info/last_token_usage"))
                && let Some(message) = messages.iter_mut().find(|message| message.id == id)
            {
                message.input_tokens = Some(input);
                message.output_tokens = Some(output);
            }
        }
        Some("task_started" | "task_complete" | "user_message") => {
            *pending = None;
        }
        _ => {}
    }
}

fn codex_usage_pair(usage: Option<&Value>) -> Option<(u64, u64)> {
    let usage = usage?;
    // Cached input and reasoning output are already included in these totals.
    Some((
        usage.get("input_tokens")?.as_u64()?,
        usage.get("output_tokens")?.as_u64()?,
    ))
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
    meta: EntryMeta<'_>,
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
        agent_name: meta.agent_name.map(ToOwned::to_owned),
        input_tokens: meta.input_tokens,
        output_tokens: meta.output_tokens,
        compaction: None,
    });
    cap_parse_messages(messages);
}

fn push_tool(
    messages: &mut Vec<TranscriptMessage>,
    name: &str,
    input: Option<String>,
    id: Option<&str>,
    timestamp: Option<&str>,
    meta: EntryMeta<'_>,
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
        agent_name: meta.agent_name.map(ToOwned::to_owned),
        input_tokens: meta.input_tokens,
        output_tokens: meta.output_tokens,
        compaction: None,
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
        agent_name: None,
        input_tokens: None,
        output_tokens: None,
        compaction: None,
    });
    cap_parse_messages(messages);
}

/// Records one compaction as its own entry. Claude writes a boundary record
/// followed by the summary it continues from; the summary fills the
/// boundary's entry instead of appearing as a huge operator message.
fn push_compaction(
    messages: &mut Vec<TranscriptMessage>,
    summary: Option<String>,
    detail: Option<CompactionDetail>,
    id: Option<&str>,
    timestamp: Option<&str>,
) {
    let summary = summary
        .filter(|text| !text.trim().is_empty())
        .map(|text| truncate_utf8(text, MAX_COMPACTION_SUMMARY_BYTES));
    if detail.is_none()
        && let Some(summary) = &summary
        && let Some(last) = messages.last_mut()
        && last.kind == "compaction"
        && last.markdown.is_empty()
    {
        last.markdown.clone_from(summary);
        return;
    }
    let id = bounded_id(id, || format!("compaction-{}", messages.len()));
    if messages.last().is_some_and(|message| message.id == id) {
        return;
    }
    messages.push(TranscriptMessage {
        id,
        role: "system".to_owned(),
        kind: "compaction".to_owned(),
        markdown: summary.unwrap_or_default(),
        tool_name: None,
        tool_input: None,
        tool_output: None,
        timestamp: timestamp.map(|value| truncate_plain(value, MAX_TIMESTAMP_BYTES)),
        agent_name: None,
        input_tokens: None,
        output_tokens: None,
        compaction: Some(detail.unwrap_or_default()),
    });
    cap_parse_messages(messages);
}

/// Claude's `compact_boundary` system record and the `isCompactSummary` user
/// turn that follows it. Returns whether `value` was one of them.
fn push_claude_compaction(
    messages: &mut Vec<TranscriptMessage>,
    value: &Value,
    id: Option<&str>,
    timestamp: Option<&str>,
) -> bool {
    if value.get("type").and_then(Value::as_str) == Some("system")
        && value.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
    {
        push_compaction(
            messages,
            None,
            Some(claude_compaction_detail(value)),
            id,
            timestamp,
        );
        return true;
    }
    if value.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
        push_compaction(messages, claude_user_text(value), None, id, timestamp);
        return true;
    }
    false
}

/// Codex's `compacted` record. Its replacement history is opaque, so the
/// entry carries a summary only when Codex wrote a plain-text one.
fn push_codex_compaction(messages: &mut Vec<TranscriptMessage>, value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("compacted") {
        return false;
    }
    let timestamp = value.get("timestamp").and_then(Value::as_str);
    let id = timestamp.map(|timestamp| format!("compaction:{timestamp}"));
    push_compaction(
        messages,
        value
            .pointer("/payload/message")
            .and_then(Value::as_str)
            .map(str::to_owned),
        Some(CompactionDetail::default()),
        id.as_deref(),
        timestamp,
    );
    true
}

fn claude_compaction_detail(value: &Value) -> CompactionDetail {
    let metadata = value.get("compactMetadata");
    let field = |key: &str| metadata.and_then(|metadata| metadata.get(key));
    CompactionDetail {
        trigger: field("trigger")
            .and_then(Value::as_str)
            .filter(|trigger| {
                trigger.len() <= 32
                    && trigger
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            })
            .map(str::to_owned),
        pre_tokens: field("preTokens").and_then(Value::as_u64),
        post_tokens: field("postTokens").and_then(Value::as_u64),
    }
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
            description: None,
            attached: false,
            windows: 1,
            activity: 0,
            output_activity: 0,
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
            session_key: None,
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
    fn claude_streamed_request_usage_is_counted_once_with_final_counters() {
        let record = |uuid: &str, content: Value, output: u64| {
            serde_json::json!({
                "type": "assistant", "uuid": uuid, "message": {
                    "id": "request-1", "role": "assistant", "content": content,
                    "usage": {"input_tokens": 100, "cache_read_input_tokens": 900,
                        "cache_creation_input_tokens": 50, "output_tokens": output}
                }
            })
            .to_string()
        };
        let source = [
            record(
                "thinking",
                serde_json::json!([{"type":"thinking","thinking":"hidden"}]),
                1,
            ),
            record(
                "text",
                serde_json::json!([{"type":"text","text":"Working"}]),
                5,
            ),
            record(
                "tool",
                serde_json::json!([{"type":"tool_use","id":"call","name":"Bash","input":{}}]),
                20,
            ),
        ]
        .join("\n");
        let (messages, _) = parse_claude(&tail(&source));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].input_tokens, Some(1050));
        assert_eq!(messages[0].output_tokens, Some(20));
        assert_eq!(messages[1].input_tokens, None);
        assert_eq!(messages[1].output_tokens, None);
    }

    #[test]
    fn transcript_usage_preserves_zero_and_rejects_invalid_or_overflowed_counters() {
        let usage = |value: Value| claude_usage(&serde_json::json!({"message":{"usage":value}}));
        assert_eq!(
            usage(serde_json::json!({"input_tokens":0,"output_tokens":0})),
            Some((0, 0))
        );
        assert_eq!(usage(serde_json::json!({"output_tokens":1})), None);
        assert_eq!(
            usage(serde_json::json!({"input_tokens":1,"output_tokens":-1})),
            None
        );
        assert_eq!(
            usage(
                serde_json::json!({"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":null})
            ),
            None
        );
        assert_eq!(
            usage(
                serde_json::json!({"input_tokens":u64::MAX,"output_tokens":1,"cache_read_input_tokens":1})
            ),
            None
        );
    }

    #[test]
    fn codex_request_tokens_attach_once_without_recounting_cached_or_duplicate_usage() {
        let call = |id: &str| {
            serde_json::json!({"type":"response_item","payload":{
                "type":"function_call","call_id":id,"name":"exec","arguments":"{}"
            }})
            .to_string()
        };
        let usage = |total: u64, input: u64, output: u64| {
            serde_json::json!({
                "type":"event_msg","payload":{"type":"token_count","info":{
                    "total_token_usage":{"input_tokens":total,"output_tokens":total},
                    "last_token_usage":{"input_tokens":input,"cached_input_tokens":900,
                        "output_tokens":output,"reasoning_output_tokens":20}
                }}
            })
            .to_string()
        };
        let source = [
            usage(100, 9999, 9999), // The preceding request is outside this tail.
            call("first"), call("same-request"), usage(110, 1000, 50),
            call("next"), usage(110, 1000, 50), // Rate-limit refresh, not a new request.
            usage(120, 500, 25),
            call("malformed"),
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":-1,"output_tokens":5}}}}"#.to_owned(),
            call("zero"), usage(130, 0, 0),
        ].join("\n");
        let (messages, _) = parse_codex(&tail(&source));
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].input_tokens, Some(1000));
        assert_eq!(messages[0].output_tokens, Some(50));
        assert_eq!(messages[1].input_tokens, None);
        assert_eq!(messages[2].input_tokens, Some(500));
        assert_eq!(messages[2].output_tokens, Some(25));
        assert_eq!(messages[3].input_tokens, None);
        assert_eq!(messages[4].input_tokens, Some(0));
        assert_eq!(
            messages
                .iter()
                .filter_map(|message| message.input_tokens)
                .sum::<u64>(),
            1500
        );
    }

    #[test]
    fn codex_usage_never_crosses_a_user_or_compact_boundary() {
        let source = concat!(
            r#"{"type":"response_item","payload":{"type":"function_call","call_id":"old","name":"exec"}}"#,
            "\n",
            r#"{"type":"compacted"}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":99,"output_tokens":2}}}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","id":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":50,"output_tokens":1}}}}"#,
            "\n",
        );
        let (messages, _) = parse_codex(&tail(source));
        assert_eq!(
            messages
                .iter()
                .map(|message| message.kind.as_str())
                .collect::<Vec<_>>(),
            ["tool", "compaction", "message"]
        );
        assert!(
            messages
                .iter()
                .all(|message| message.input_tokens.is_none())
        );
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
    fn claude_subagent_turns_are_attributed_to_the_subagent_and_carry_request_usage() {
        let source = concat!(
            r#"{"type":"user","uuid":"human","message":{"role":"user","content":"do the thing"}}"#,
            "\n",
            r#"{"type":"user","uuid":"agent-report","turnCompanion":true,"agentName":"Explore","message":{"role":"user","content":[{"type":"text","text":"Searched the tree and found three call sites."}]}}"#,
            "\n",
            r#"{"type":"user","uuid":"tool-authored","sourceToolAssistantUUID":"a1","message":{"role":"user","content":"Running in the background as @scout"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","message":{"role":"assistant","usage":{"input_tokens":40,"cache_read_input_tokens":1000,"output_tokens":7},"content":[{"type":"text","text":"Summarising."},{"type":"tool_use","id":"call-1","name":"Bash","input":{"command":"ls"}}]}}"#,
            "\n",
        );
        let (messages, _) = parse_claude(&tail(source));
        assert_eq!(
            messages
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            ["user", "subagent", "subagent", "assistant", "tool"]
        );
        assert_eq!(messages[1].agent_name.as_deref(), Some("Explore"));
        assert_eq!(messages[2].agent_name, None);
        // One record is one request, so only its first entry spends the usage.
        assert_eq!(messages[3].input_tokens, Some(1_040));
        assert_eq!(messages[3].output_tokens, Some(7));
        assert_eq!(messages[4].input_tokens, None);
    }

    #[test]
    fn claude_task_notification_reads_as_a_named_subagent_report() {
        // Recorded verbatim from Claude Code 2.1.261: the Agent tool's
        // completion turn carries no tool markers, only a harness origin.
        let source = include_str!("../tests/fixtures/claude-task-notification.jsonl");
        let (messages, _) = parse_claude(&tail(source));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "subagent");
        assert_eq!(messages[0].agent_name.as_deref(), Some("Reply with PONG"));
        assert_eq!(
            messages[0].markdown,
            "Agent \"Reply with PONG\" finished\n\nPONG"
        );
        assert!(!messages[0].markdown.contains("<task-notification>"));
        assert!(
            !messages[0]
                .markdown
                .contains("tasks/afccf69c7fd0edf4f.output")
        );
    }

    #[test]
    fn claude_harness_authored_user_lines_never_read_as_the_operator() {
        let source = concat!(
            r#"{"type":"user","uuid":"typed","promptSource":"typed","origin":{"kind":"human"},"message":{"role":"user","content":"do the thing"}}"#,
            "\n",
            r#"{"type":"user","uuid":"origin-only","origin":{"kind":"task-notification"},"message":{"role":"user","content":"Agent finished"}}"#,
            "\n",
            r#"{"type":"user","uuid":"prompt-source","promptSource":"system","message":{"role":"user","content":"queued follow-up"}}"#,
            "\n",
            r#"{"type":"user","uuid":"envelope","message":{"role":"user","content":[{"type":"text","text":"<task-notification><status>completed</status><result>done</result></task-notification>"}]}}"#,
            "\n",
        );
        let (messages, _) = parse_claude(&tail(source));
        assert_eq!(
            messages
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            ["user", "subagent", "subagent", "subagent"]
        );
        assert_eq!(messages[3].agent_name.as_deref(), Some("Task"));
        assert_eq!(messages[3].markdown, "done");
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

    fn write_claude_metadata(root: &Path, pid: u32, fields: &Value) {
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::write(
            root.join(format!("sessions/{pid}.json")),
            fields.to_string(),
        )
        .unwrap();
    }

    fn write_claude_log(root: &Path, cwd: &Path, session_id: &str) -> PathBuf {
        let log = claude_log_path(root, cwd, session_id);
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, "{}\n").unwrap();
        log
    }

    #[test]
    fn claude_startup_prompt_delay_maps_through_the_exact_process_start() {
        // A development-channels or trust prompt answered minutes after launch
        // makes Claude record a late startedAt. The OS process start it also
        // records is exact, so the pane still maps.
        let home = fixture_root("claude-proc-start");
        let cwd = home.join("work");
        fs::create_dir_all(&cwd).unwrap();
        let root = home.join(".claude-max");
        let pid = std::process::id();
        let session_id = "11111111-1111-1111-1111-111111111111";
        let log = write_claude_log(&root, &cwd, session_id);
        let selected = session(AgentKind::Claude, cwd.clone(), pid, 1_000);
        let stamp = crate::control::native_process_start_stamp(pid)
            .expect("the test process has a native start stamp");
        let recorded = stamp.split_once(':').unwrap().1.to_owned();
        let metadata = |proc_start: &str, tmux: &str| {
            serde_json::json!({
                "pid": pid, "cwd": cwd.display().to_string(), "sessionId": session_id,
                "startedAt": 1_000 + 69 * 60 * 1_000, "procStart": proc_start, "tmux": tmux,
            })
        };

        write_claude_metadata(&root, pid, &metadata(&recorded, "fixture:@1.%1"));
        assert_eq!(locate_claude(&selected, &home), Some(log.clone()));

        // A different process start is PID reuse, not this process.
        write_claude_metadata(&root, pid, &metadata("1", "fixture:@1.%1"));
        assert_eq!(
            claude_lookup_in_home(&selected, &home),
            ClaudeLookup::Unmapped
        );

        // Metadata recorded in another tmux pane is never this pane's.
        write_claude_metadata(&root, pid, &metadata(&recorded, "other:@4.%9"));
        assert_eq!(
            claude_lookup_in_home(&selected, &home),
            ClaudeLookup::Unmapped
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn process_start_stamps_compare_exactly_per_platform() {
        assert!(process_start_stamp_matches("linux:5561", "5561"));
        assert!(!process_start_stamp_matches("linux:5561", "5562"));
        assert!(process_start_stamp_matches(
            "macos:Wed Sep  3 01:27:49 2026",
            "Wed Sep 3 01:27:49 2026"
        ));
        assert!(!process_start_stamp_matches(
            "macos:Wed Sep 30 01:27:49 2026",
            "Wed Sep 30 01:27:50 2026"
        ));
        assert!(!process_start_stamp_matches("linux:5561", " "));
        assert!(!process_start_stamp_matches("other:5561", "5561"));
    }

    #[test]
    fn claude_pane_binding_rejects_only_a_parsed_different_pane() {
        let binding = |value: Value| serde_json::json!({ "tmux": value });
        assert!(claude_metadata_pane_matches(&serde_json::json!({}), "%1"));
        assert!(claude_metadata_pane_matches(&binding(Value::Null), "%1"));
        assert!(claude_metadata_pane_matches(
            &binding("nes:@12.%12".into()),
            "%12"
        ));
        assert!(!claude_metadata_pane_matches(
            &binding("nes:@12.%12".into()),
            "%13"
        ));
        // A renamed session keeps its pane id; only the pane is compared.
        assert!(claude_metadata_pane_matches(
            &binding("old.name:@1.%3".into()),
            "%3"
        ));
        assert!(claude_metadata_pane_matches(
            &binding("unparseable".into()),
            "%1"
        ));
    }

    #[test]
    fn claude_pid_domain_from_another_machine_or_namespace_never_matches() {
        let machine = Some("fea822b9e425493fb68a870ffa2e2b48");
        let namespace = Some("pid:[4026531836]");
        let recorded = "linux:fea822b9e425493fb68a870ffa2e2b48:pid:[4026531836]";
        assert!(linux_pid_domain_matches(recorded, machine, namespace));
        assert!(linux_pid_domain_matches(recorded, None, None));
        assert!(!linux_pid_domain_matches(
            "linux:00000000000000000000000000000000:pid:[4026531836]",
            machine,
            namespace
        ));
        assert!(!linux_pid_domain_matches(
            "linux:fea822b9e425493fb68a870ffa2e2b48:pid:[4026532001]",
            machine,
            namespace
        ));
        assert!(!linux_pid_domain_matches("darwin", machine, namespace));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_pid_domain_of_this_process_matches() {
        let machine = fs::read_to_string("/etc/machine-id").unwrap_or_default();
        let namespace = fs::read_link("/proc/self/ns/pid").unwrap();
        let domain = format!("linux:{}:{}", machine.trim(), namespace.to_str().unwrap());
        assert!(claude_pid_domain_matches(
            &serde_json::json!({ "pidDomain": domain })
        ));
        assert!(claude_pid_domain_matches(&serde_json::json!({})));
        assert!(!claude_pid_domain_matches(
            &serde_json::json!({ "pidDomain": "darwin" })
        ));
    }

    #[test]
    fn claude_session_without_a_log_is_mapped_but_empty() {
        let home = fixture_root("claude-empty-session");
        let cwd = home.join("work");
        fs::create_dir_all(&cwd).unwrap();
        let root = home.join(".claude-hd-max");
        let session_id = "11111111-1111-1111-1111-111111111111";
        write_claude_metadata(
            &root,
            42,
            &serde_json::json!({
                "pid": 42, "cwd": cwd.display().to_string(), "startedAt": 1_000,
                "sessionId": session_id,
            }),
        );
        let selected = session(AgentKind::Claude, cwd.clone(), 42, 1_000);
        let expected = claude_log_path(&root, &cwd, session_id);
        assert_eq!(
            claude_lookup_in_home(&selected, &home),
            ClaudeLookup::AwaitingFirstMessage(expected.clone())
        );
        // Mutating callers still see no target until the log exists.
        assert_eq!(claude_resume_target_in_home(&selected, &home), None);

        let first = empty_transcript("claude", &expected, None, NOTE_CLAUDE_EMPTY);
        assert!(first.available && first.changed);
        assert_eq!(first.messages, Some(Vec::new()));
        assert_eq!(first.note.as_deref(), Some(NOTE_CLAUDE_EMPTY));
        let again = empty_transcript(
            "claude",
            &expected,
            Some(&first.content_hash),
            NOTE_CLAUDE_EMPTY,
        );
        assert!(!again.changed && again.messages.is_none());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn claude_still_starting_resumes_only_an_explicit_existing_log() {
        let home = fixture_root("claude-starting");
        let cwd = home.join("work");
        fs::create_dir_all(&cwd).unwrap();
        let root = home.join(".claude-max");
        fs::create_dir_all(root.join("sessions")).unwrap();
        let session_id = "11111111-1111-1111-1111-111111111111";
        let log = write_claude_log(&root, &cwd, session_id);
        let pid = std::process::id();
        let selected = session(AgentKind::Claude, cwd.clone(), pid, 1_000);
        let roots = claude_roots("claude", &home).unwrap();
        let argv = |arguments: &[&str]| {
            std::iter::once("claude")
                .chain(arguments.iter().copied())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };

        // No metadata anywhere: this test process has no --resume argument.
        assert_eq!(
            claude_lookup_in_home(&selected, &home),
            ClaudeLookup::Starting { resuming: None }
        );
        for accepted in [
            argv(&[
                "--dangerously-load-development-channels",
                "server:x",
                "--resume",
                session_id,
            ]),
            argv(&[&format!("--resume={session_id}")]),
            argv(&["-r", session_id, "--model", "opus"]),
            argv(&["--session-id", session_id, "--resume", session_id]),
        ] {
            assert_eq!(
                claude_resume_log_for_argv(&accepted, &selected, &roots),
                Some(log.clone()),
                "{accepted:?}"
            );
        }
        let other = "22222222-2222-2222-2222-222222222222";
        for rejected in [
            argv(&["--resume"]),
            argv(&["--resume", "--model", "opus"]),
            argv(&["--continue"]),
            argv(&["--resume", session_id, "--fork-session"]),
            argv(&["--resume", session_id, "--session-id", other]),
            argv(&["--resume", other]),
            argv(&["--", "--resume", session_id]),
        ] {
            assert_eq!(
                claude_resume_log_for_argv(&rejected, &selected, &roots),
                None,
                "{rejected:?}"
            );
        }

        // Rejected live metadata never falls back to argv.
        write_claude_metadata(
            &root,
            pid,
            &serde_json::json!({"pid": pid, "cwd": "/elsewhere"}),
        );
        assert_eq!(
            claude_lookup_in_home(&selected, &home),
            ClaudeLookup::Unmapped
        );
        fs::remove_dir_all(home).unwrap();
    }

    fn write_rollout(directory: &Path, id: &str, cwd: &Path, thread_source: &str) -> PathBuf {
        fs::create_dir_all(directory).unwrap();
        let path = directory.join(format!("rollout-2026-09-29T14-43-03-{id}.jsonl"));
        fs::write(
            &path,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{id}","source":"cli","thread_source":"{thread_source}","cwd":"{}"}}}}"#,
                cwd.display()
            ) + "\n",
        )
        .unwrap();
        path
    }

    /// A `UUIDv7` created at `ms`.
    fn v7(ms: u64) -> String {
        let hex = format!("{ms:012x}");
        format!("{}-{}-7000-8000-000000000000", &hex[..8], &hex[8..])
    }

    #[test]
    fn uuid_v7_times_and_civil_days_are_exact() {
        assert_eq!(
            uuid_v7_millis("01a0eee8-199d-79c2-a39f-091c4eff95ba"),
            Some(0x01a0_eee8_199d)
        );
        assert_eq!(uuid_v7_millis("11111111-1111-1111-1111-111111111111"), None);
        assert_eq!(
            uuid_v7_millis(&v7(1_790_000_000_000)),
            Some(1_790_000_000_000)
        );
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_726), (2026, 9, 30));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn codex_resume_argv_names_one_thread() {
        let argv = |words: &str| words.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let id = "01a0eee8-199d-79c2-a39f-091c4eff95ba";
        assert_eq!(
            codex_argv_resume_id(&argv(&format!("codex resume {id}"))).as_deref(),
            Some(id)
        );
        assert_eq!(
            codex_argv_resume_id(&argv(&format!(
                "codex -c check=false resume -C /work -m gpt {id}"
            )))
            .as_deref(),
            Some(id)
        );
        assert_eq!(codex_argv_resume_id(&argv("codex resume --last")), None);
        assert_eq!(codex_argv_resume_id(&argv("codex resume")), None);
        assert_eq!(codex_argv_resume_id(&argv(&format!("codex {id}"))), None);
        assert_eq!(
            codex_argv_resume_id(&argv(&format!("codex resume {id} {}", v7(1)))),
            None
        );
    }

    #[test]
    fn codex_resumed_rollout_is_shown_until_a_newer_thread_exists() {
        let home = fixture_root("codex-resumed");
        let root = home.join(".codex");
        let cwd = home.join("work");
        fs::create_dir_all(&cwd).unwrap();
        let day_ms = 86_400_000;
        let created = 20_726 * day_ms + 12 * 3_600_000; // 2026-09-30 12:00 UTC
        let started = created + 3 * day_ms; // 2026-10-03
        let now = started + 2 * 3_600_000;
        let id = v7(created);
        let target = write_rollout(&root.join("sessions/2026/09/30"), &id, &cwd, "user");
        let selected = session(AgentKind::Codex, cwd.clone(), 42, started);
        let argv = vec!["codex".to_owned(), "resume".to_owned(), id.clone()];

        assert_eq!(
            codex_resumed_rollout_for_argv(&argv, &selected, &root, now),
            Some(target.clone())
        );

        // An older same-directory thread and a subagent thread change nothing.
        write_rollout(
            &root.join("sessions/2026/09/29"),
            &v7(created - day_ms),
            &cwd,
            "user",
        );
        write_rollout(
            &root.join("sessions/2026/10/03"),
            &v7(started + 1),
            &cwd,
            "subagent",
        );
        write_rollout(
            &root.join("sessions/2026/10/03"),
            &v7(started + 2),
            &home,
            "user",
        );
        write_rollout(
            &root.join("sessions/2026/10/02"),
            &v7(started - 60_000),
            &cwd,
            "user",
        );
        assert_eq!(
            codex_resumed_rollout_for_argv(&argv, &selected, &root, now),
            Some(target)
        );

        // A user thread in this directory created after the process started
        // (a /new, or a sibling process) makes the mapping ambiguous.
        write_rollout(
            &root.join("sessions/2026/10/03"),
            &v7(started + 3),
            &cwd,
            "user",
        );
        assert_eq!(
            codex_resumed_rollout_for_argv(&argv, &selected, &root, now),
            None
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn codex_resume_fallback_refuses_an_unbounded_scan() {
        let sessions = Path::new("/nonexistent/sessions");
        let day_ms = 86_400_000;
        assert!(codex_day_directories(sessions, 100 * day_ms, 130 * day_ms).is_some());
        assert!(codex_day_directories(sessions, 100 * day_ms, 200 * day_ms).is_none());
    }

    #[test]
    fn claude_compaction_is_one_entry_carrying_its_summary_and_counts() {
        let source = concat!(
            r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"before"}}"#,
            "\n",
            r#"{"type":"system","subtype":"compact_boundary","uuid":"b1","timestamp":"t1","content":"Conversation compacted","compactMetadata":{"trigger":"manual","preTokens":245549,"postTokens":11832}}"#,
            "\n",
            r#"{"type":"user","uuid":"s1","isCompactSummary":true,"isVisibleInTranscriptOnly":true,"message":{"role":"user","content":"This session is being continued from a previous conversation. Summary: ..."}}"#,
            "\n",
            r#"{"type":"user","uuid":"u2","message":{"role":"user","content":"after"}}"#,
            "\n",
        );
        let (messages, _) = parse_claude(&tail(source));
        assert_eq!(
            messages
                .iter()
                .map(|message| (message.role.as_str(), message.kind.as_str()))
                .collect::<Vec<_>>(),
            [
                ("user", "message"),
                ("system", "compaction"),
                ("user", "message")
            ]
        );
        let compaction = &messages[1];
        assert_eq!(compaction.id, "b1");
        assert!(
            compaction
                .markdown
                .starts_with("This session is being continued")
        );
        assert_eq!(
            compaction.compaction,
            Some(CompactionDetail {
                trigger: Some("manual".to_owned()),
                pre_tokens: Some(245_549),
                post_tokens: Some(11_832),
            })
        );

        // A summary whose boundary fell outside the bounded tail still
        // becomes a compaction entry, never an operator message.
        let (messages, _) = parse_claude(&tail(concat!(
            r#"{"type":"user","uuid":"s1","isCompactSummary":true,"message":{"role":"user","content":"Summary"}}"#,
            "\n",
        )));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, "compaction");
        assert_eq!(messages[0].markdown, "Summary");
    }

    #[test]
    fn codex_compaction_is_a_marker_entry() {
        let source = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","id":"u","content":[{"type":"input_text","text":"hello"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-09-12T21:54:25.178Z","type":"compacted","payload":{"message":"","replacement_history":[]}}"#,
            "\n",
        );
        let (messages, _) = parse_codex(&tail(source));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].kind, "compaction");
        assert_eq!(messages[1].id, "compaction:2026-09-12T21:54:25.178Z");
        assert!(messages[1].markdown.is_empty());
        assert_eq!(messages[1].compaction, Some(CompactionDetail::default()));
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
                agent_name: None,
                input_tokens: None,
                output_tokens: None,
                compaction: None,
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
                agent_name: None,
                input_tokens: None,
                output_tokens: None,
                compaction: None,
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
                EntryMeta::default(),
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
