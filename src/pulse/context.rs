//! Bounded Claude session context-window collection.
//!
//! Claude stores top-level sessions under
//! `<CLAUDE_CONFIG_DIR>/projects/<project>/*.jsonl`. Subagent and tool-result
//! directories are deliberately ignored because their context belongs to a
//! parent session.

use std::{
    collections::BTreeMap,
    fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant as MonotonicInstant, UNIX_EPOCH},
};

use serde_json::Value;

use super::{
    AccountId, AgentSettings, ContextSession, Instant, MachineName, Profile, ProfileName,
    PulseError, PulseErrorKind, PulseResult, SessionId, Vendor, collect::open_regular_bounded,
    model::Percent,
};

pub const DEFAULT_CONTEXT_LIMIT: u64 = 200_000;
pub const LARGE_CONTEXT_LIMIT: u64 = 1_000_000;
pub const COMPACT_RECOMMEND_PERCENT: u64 = 75;
pub const DEFAULT_CONTEXT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

const TAIL_BYTES: u64 = 256 * 1024;
const MAX_PROJECTS: usize = 4_096;
const MAX_DIRECTORY_ENTRIES: usize = 100_000;
const MAX_SESSION_FILES: usize = 512;
const MAX_TOTAL_TAIL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SCAN_DURATION: Duration = Duration::from_secs(12);
const MAX_FUTURE_MTIME_MILLIS: i64 = 5 * 60 * 1_000;

// Longest/specific prefixes must precede their older-family prefixes.
const MODEL_LIMITS: &[(&str, u64)] = &[
    ("claude-fable-5[1m]", LARGE_CONTEXT_LIMIT),
    ("claude-fable-5-1m", LARGE_CONTEXT_LIMIT),
    ("claude-fable-5", LARGE_CONTEXT_LIMIT),
    ("claude-mythos-5", LARGE_CONTEXT_LIMIT),
    ("claude-opus-4-8[1m]", LARGE_CONTEXT_LIMIT),
    ("claude-opus-4-8-1m", LARGE_CONTEXT_LIMIT),
    ("claude-opus-4-8", LARGE_CONTEXT_LIMIT),
    ("claude-opus-4-7[1m]", LARGE_CONTEXT_LIMIT),
    ("claude-opus-4-7-1m", LARGE_CONTEXT_LIMIT),
    ("claude-opus-4-7", LARGE_CONTEXT_LIMIT),
    ("claude-opus-4-6", LARGE_CONTEXT_LIMIT),
    ("claude-opus-4-5", DEFAULT_CONTEXT_LIMIT),
    ("claude-opus-4-1", DEFAULT_CONTEXT_LIMIT),
    ("claude-opus-4", DEFAULT_CONTEXT_LIMIT),
    ("claude-sonnet-4-6", LARGE_CONTEXT_LIMIT),
    ("claude-sonnet-4-5", DEFAULT_CONTEXT_LIMIT),
    ("claude-sonnet-4-1", DEFAULT_CONTEXT_LIMIT),
    ("claude-sonnet-4", DEFAULT_CONTEXT_LIMIT),
    ("claude-haiku-4-5", DEFAULT_CONTEXT_LIMIT),
    ("claude-haiku-4-1", DEFAULT_CONTEXT_LIMIT),
];

/// Per-profile context rows plus the count of individual unsafe/unusable files.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextCollection {
    pub sessions: Vec<ContextSession>,
    pub failures: usize,
}

/// Returns the effective context limit for a Claude model identifier.
#[must_use]
pub fn effective_context_for_model(model: Option<&str>) -> u64 {
    let Some(model) = model else {
        return DEFAULT_CONTEXT_LIMIT;
    };
    let normalized = model.to_ascii_lowercase();
    MODEL_LIMITS
        .iter()
        .find(|(prefix, _)| normalized.starts_with(prefix))
        .map_or(DEFAULT_CONTEXT_LIMIT, |(_, limit)| *limit)
}

/// Collects every recently active top-level Claude session for one profile.
///
/// # Errors
///
/// Returns a configuration/work-bound error for an unsafe profile root or an
/// excessive directory scan. Individual session failures are counted and do
/// not suppress healthy sibling sessions.
pub fn collect_profile_contexts(
    profile: &Profile,
    machine: &MachineName,
    collected_at: Instant,
    max_age: Duration,
) -> PulseResult<ContextCollection> {
    if profile.vendor != Vendor::AnthropicOauth {
        return Ok(ContextCollection::default());
    }
    if max_age.is_zero() || max_age > Duration::from_secs(31 * 24 * 60 * 60) {
        return Err(PulseError::invalid_input(
            "context session max age must be between one second and 31 days",
        ));
    }
    let config_dir = profile.config_dir.as_deref().ok_or_else(|| {
        PulseError::configuration("Claude context collection requires an explicit config_dir")
    })?;
    if !config_dir.is_absolute() {
        return Err(PulseError::configuration(
            "Claude context config_dir must be absolute",
        ));
    }
    let max_age_ms = i64::try_from(max_age.as_millis())
        .map_err(|_| PulseError::invalid_input("context max age overflowed"))?;
    let cutoff = collected_at.epoch_millis().saturating_sub(max_age_ms);
    let files = discover_session_files(config_dir, cutoff)?;
    let future_limit = collected_at
        .epoch_millis()
        .saturating_add(MAX_FUTURE_MTIME_MILLIS);
    let started = MonotonicInstant::now();
    let mut total_tail_bytes = 0_u64;
    let mut failures = 0_usize;
    let mut by_session = BTreeMap::<String, ContextSession>::new();

    for path in files {
        if started.elapsed() > MAX_SCAN_DURATION {
            return Err(PulseError::new(
                PulseErrorKind::Storage,
                "Claude context collection exceeded its time bound",
            ));
        }
        match read_session(
            &path,
            profile.account_id,
            profile.name.clone(),
            machine.clone(),
            collected_at,
            cutoff,
            future_limit,
            &mut total_tail_bytes,
        ) {
            Ok(Some(session)) => {
                let key = session.session_id.as_str().to_owned();
                let replace = by_session
                    .get(&key)
                    .is_none_or(|current| session.last_active_at > current.last_active_at);
                if replace {
                    by_session.insert(key, session);
                }
            }
            Ok(None) => {}
            Err(_) => failures = failures.saturating_add(1),
        }
    }
    let mut sessions = by_session.into_values().collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .last_active_at
            .cmp(&left.last_active_at)
            .then_with(|| left.session_id.as_str().cmp(right.session_id.as_str()))
    });
    Ok(ContextCollection { sessions, failures })
}

fn discover_session_files(config_dir: &Path, cutoff: i64) -> PulseResult<Vec<PathBuf>> {
    real_directory(
        config_dir,
        "Claude context config_dir is unavailable or unsafe",
    )?;
    let projects = config_dir.join("projects");
    match fs::symlink_metadata(&projects) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => {
            return Err(PulseError::new(
                PulseErrorKind::Storage,
                "Claude projects directory could not be inspected",
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(PulseError::configuration(
                "Claude projects directory must be a real directory",
            ));
        }
        Ok(_) => {}
    }
    let started = MonotonicInstant::now();
    let mut entries_seen = 0_usize;
    let mut projects_seen = 0_usize;
    let mut files = Vec::new();
    let project_entries = fs::read_dir(&projects).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Claude projects directory could not be read",
        )
    })?;
    for project in project_entries {
        check_scan_bounds(started, entries_seen)?;
        entries_seen = entries_seen.saturating_add(1);
        let project = project.map_err(|_| {
            PulseError::new(
                PulseErrorKind::Storage,
                "Claude project entry could not be read",
            )
        })?;
        let file_type = project.file_type().map_err(|_| {
            PulseError::new(
                PulseErrorKind::Storage,
                "Claude project entry type could not be read",
            )
        })?;
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        if collect_recent_project_sessions(
            &project.path(),
            cutoff,
            started,
            &mut entries_seen,
            &mut files,
        )? {
            projects_seen = projects_seen.saturating_add(1);
            if projects_seen > MAX_PROJECTS {
                return Err(work_bound_error());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn collect_recent_project_sessions(
    project: &Path,
    cutoff: i64,
    started: MonotonicInstant,
    entries_seen: &mut usize,
    files: &mut Vec<PathBuf>,
) -> PulseResult<bool> {
    let sessions = fs::read_dir(project).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Claude project session directory could not be read",
        )
    })?;
    let files_before = files.len();
    for session in sessions {
        check_scan_bounds(started, *entries_seen)?;
        *entries_seen = entries_seen.saturating_add(1);
        let session = session.map_err(|_| {
            PulseError::new(
                PulseErrorKind::Storage,
                "Claude session entry could not be read",
            )
        })?;
        let file_type = session.file_type().map_err(|_| {
            PulseError::new(
                PulseErrorKind::Storage,
                "Claude session entry type could not be read",
            )
        })?;
        let path = session.path();
        if file_type.is_symlink()
            || !file_type.is_file()
            || !path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            continue;
        }
        let modified_ms = session
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .and_then(|value| i64::try_from(value.as_millis()).ok());
        if modified_ms.is_none_or(|modified| modified < cutoff) {
            continue;
        }
        if files.len() >= MAX_SESSION_FILES {
            return Err(work_bound_error());
        }
        files.push(path);
    }
    Ok(files.len() > files_before)
}

fn real_directory(path: &Path, message: &'static str) -> PulseResult<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(PulseError::configuration(message));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| PulseError::configuration(message))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(PulseError::configuration(message));
        }
    }
    Ok(())
}

fn check_scan_bounds(started: MonotonicInstant, entries_seen: usize) -> PulseResult<()> {
    if entries_seen >= MAX_DIRECTORY_ENTRIES || started.elapsed() > MAX_SCAN_DURATION {
        return Err(work_bound_error());
    }
    Ok(())
}

fn work_bound_error() -> PulseError {
    PulseError::new(
        PulseErrorKind::Storage,
        "Claude context file scan exceeded its work bound",
    )
}

#[allow(clippy::too_many_arguments)]
fn read_session(
    path: &Path,
    account_id: AccountId,
    profile: ProfileName,
    machine: MachineName,
    collected_at: Instant,
    cutoff: i64,
    future_limit: i64,
    total_tail_bytes: &mut u64,
) -> PulseResult<Option<ContextSession>> {
    let mut file = open_regular_bounded(path, u64::MAX).map_err(|_| unsafe_session_error())?;
    let metadata = file.metadata().map_err(|_| unsafe_session_error())?;
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .and_then(|value| i64::try_from(value.as_millis()).ok())
        .ok_or_else(unsafe_session_error)?;
    if modified_ms < cutoff {
        return Ok(None);
    }
    if modified_ms > future_limit {
        return Err(PulseError::new(
            PulseErrorKind::Storage,
            "Claude session modification time was implausibly far in the future",
        ));
    }
    let size = metadata.len();
    if size == 0 {
        return Ok(None);
    }
    let read_len = size.min(TAIL_BYTES);
    *total_tail_bytes = total_tail_bytes.saturating_add(read_len);
    if *total_tail_bytes > MAX_TOTAL_TAIL_BYTES {
        return Err(PulseError::new(
            PulseErrorKind::Storage,
            "Claude context tails exceeded their total byte bound",
        ));
    }
    let started_mid_line = size > read_len;
    if started_mid_line {
        file.seek(SeekFrom::Start(size - read_len))
            .map_err(|_| unsafe_session_error())?;
    }
    let capacity = usize::try_from(read_len).unwrap_or(usize::MAX);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(read_len)
        .read_to_end(&mut bytes)
        .map_err(|_| unsafe_session_error())?;
    let content = if started_mid_line {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(&[][..], |newline| &bytes[newline + 1..])
    } else {
        &bytes
    };
    parse_session(
        content,
        path,
        account_id,
        profile,
        machine,
        collected_at,
        Instant::from_epoch_millis(modified_ms)?,
    )
}

fn unsafe_session_error() -> PulseError {
    PulseError::new(
        PulseErrorKind::Storage,
        "Claude session was unavailable or unsafe",
    )
}

fn parse_session(
    bytes: &[u8],
    path: &Path,
    account_id: AccountId,
    profile: ProfileName,
    machine: MachineName,
    collected_at: Instant,
    last_active_at: Instant,
) -> PulseResult<Option<ContextSession>> {
    let mut usage = None::<(u64, u64, u64)>;
    let mut model = None::<String>;
    let mut session = None::<String>;
    let mut last_reset_at = None::<Instant>;
    let mut saw_object = false;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        saw_object = true;
        if let Some(found) = object.get("sessionId").and_then(Value::as_str) {
            session = Some(found.to_owned());
        }
        if object.get("type").and_then(Value::as_str) == Some("system")
            && object.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
            && let Some(timestamp) = object.get("timestamp").and_then(Value::as_str)
            && let Ok(timestamp) = Instant::from_iso8601(timestamp)
        {
            last_reset_at = Some(timestamp);
        }
        let Some(message) = object.get("message").and_then(Value::as_object) else {
            continue;
        };
        let assistant = message.get("role").and_then(Value::as_str) == Some("assistant")
            || object.get("type").and_then(Value::as_str) == Some("assistant");
        if !assistant {
            continue;
        }
        let Some(found_usage) = message.get("usage") else {
            continue;
        };
        usage = Some((
            token_value(found_usage.get("input_tokens")),
            token_value(found_usage.get("cache_creation_input_tokens")),
            token_value(found_usage.get("cache_read_input_tokens")),
        ));
        if let Some(found) = message.get("model").and_then(Value::as_str) {
            model = Some(found.to_owned());
        }
    }
    if !saw_object {
        return Ok(None);
    }
    let session = session.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .unwrap_or("unknown-session")
            .to_owned()
    });
    let context_tokens = usage
        .map(|(input, cache_write, cache_read)| {
            input
                .checked_add(cache_write)
                .and_then(|total| total.checked_add(cache_read))
                .ok_or_else(|| PulseError::invalid_input("Claude context token count overflowed"))
        })
        .transpose()?
        .unwrap_or_default();
    let effective_limit = effective_context_for_model(model.as_deref());
    if context_tokens > effective_limit {
        return Err(PulseError::new(
            PulseErrorKind::Upstream,
            "Claude context usage exceeded the effective model limit",
        ));
    }
    #[allow(clippy::cast_precision_loss)]
    let percent = (context_tokens as f64 / effective_limit as f64) * 100.0;
    let context = ContextSession {
        account_id,
        profile,
        machine,
        session_id: SessionId::new(session)?,
        model,
        settings: AgentSettings::default(),
        context_tokens: Some(context_tokens),
        context_percent: Some(Percent::new((percent * 100.0).round() / 100.0)?),
        effective_limit: Some(effective_limit),
        last_active_at,
        last_reset_at,
        collected_at,
    };
    context.validate()?;
    Ok(Some(context))
}

fn token_value(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::pulse::RefreshPolicy;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "atmux-pulse-context-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp");
            Self(path)
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
            fs::write(&path, contents).expect("write fixture");
            path
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn profile(directory: &Path) -> Profile {
        Profile {
            account_id: AccountId::new(1).expect("account"),
            name: ProfileName::new("claude-max").expect("profile"),
            vendor: Vendor::AnthropicOauth,
            config_dir: Some(directory.to_path_buf()),
            poll_interval_minutes: 15,
            monthly_budget_usd: None,
            api_key_env: None,
            api_key_file: None,
            refresh: RefreshPolicy::Never,
            hidden: false,
            origin: crate::pulse::ProfileOrigin::Local,
        }
    }

    fn assistant(session: &str, model: &str, input: u64, create: u64, read: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "sessionId": session,
            "message": {
                "role": "assistant",
                "model": model,
                "usage": {
                    "input_tokens": input,
                    "cache_creation_input_tokens": create,
                    "cache_read_input_tokens": read,
                    "output_tokens": 999_999
                }
            }
        })
        .to_string()
    }

    fn set_mtime(path: &Path, millis: u64) {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime");
        let modified = UNIX_EPOCH + Duration::from_millis(millis);
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .expect("set mtime");
    }

    #[test]
    fn model_limits_are_specific_and_case_insensitive() {
        for model in [
            "claude-fable-5",
            "CLAUDE-MYTHOS-5",
            "claude-opus-4-8-20260601",
            "claude-opus-4-7[1m]",
            "claude-sonnet-4-6",
        ] {
            assert_eq!(
                effective_context_for_model(Some(model)),
                LARGE_CONTEXT_LIMIT
            );
        }
        for model in [
            "claude-opus-4-5",
            "claude-sonnet-4-5",
            "claude-haiku-4-5-20251001",
            "unknown",
        ] {
            assert_eq!(
                effective_context_for_model(Some(model)),
                DEFAULT_CONTEXT_LIMIT
            );
        }
        assert_eq!(effective_context_for_model(None), DEFAULT_CONTEXT_LIMIT);
    }

    #[test]
    fn latest_assistant_usage_sets_context_and_compaction_boundary() {
        let temp = TempDirectory::new();
        let compact = serde_json::json!({
            "type": "system",
            "subtype": "compact_boundary",
            "timestamp": "2026-08-08T12:00:00Z"
        });
        temp.write(
            "projects/project/session.jsonl",
            &format!(
                "{}\n{}\n{}\n",
                assistant("session-1", "claude-sonnet-4-6", 100, 200, 50_000),
                compact,
                assistant("session-1", "claude-sonnet-4-6", 10, 1_000, 150_000)
            ),
        );
        let collection = collect_profile_contexts(
            &profile(&temp.0),
            &MachineName::new("midnight").expect("machine"),
            Instant::now(),
            DEFAULT_CONTEXT_MAX_AGE,
        )
        .expect("collect");
        assert_eq!(collection.failures, 0);
        assert_eq!(collection.sessions.len(), 1);
        let context = &collection.sessions[0];
        assert_eq!(context.context_tokens, Some(151_010));
        assert_eq!(context.effective_limit, Some(1_000_000));
        assert_eq!(context.context_percent.map(Percent::get), Some(15.1));
        assert_eq!(
            context.last_reset_at,
            Some(Instant::from_iso8601("2026-08-08T12:00:00Z").expect("reset"))
        );
    }

    #[test]
    fn recent_context_filters_old_sessions_before_session_file_cap() {
        let temp = TempDirectory::new();
        let collected_at = Instant::now();
        let old_millis = u64::try_from(
            collected_at
                .epoch_millis()
                .saturating_sub(2 * 24 * 60 * 60 * 1_000),
        )
        .expect("current time is positive");
        for index in 0..=MAX_SESSION_FILES {
            let path = temp.write(&format!("projects/project/old-{index}.jsonl"), "{}\n");
            set_mtime(&path, old_millis);
        }
        temp.write(
            "projects/project/recent.jsonl",
            &format!("{}\n", assistant("recent", "claude-opus-4-8", 1, 2, 3)),
        );

        let collection = collect_profile_contexts(
            &profile(&temp.0),
            &MachineName::new("tron").expect("machine"),
            collected_at,
            DEFAULT_CONTEXT_MAX_AGE,
        )
        .expect("recent context ignores old sessions before file cap");

        assert_eq!(collection.failures, 0);
        assert_eq!(collection.sessions.len(), 1);
        assert_eq!(collection.sessions[0].session_id.as_str(), "recent");
    }

    #[test]
    fn empty_historical_project_directories_do_not_consume_recent_project_cap() {
        let temp = TempDirectory::new();
        for index in 0..=MAX_PROJECTS {
            fs::create_dir_all(temp.0.join(format!("projects/old-project-{index}")))
                .expect("create historical project directory");
        }
        temp.write(
            "projects/current/recent.jsonl",
            &format!("{}\n", assistant("recent", "claude-opus-4-8", 1, 2, 3)),
        );

        let collection = collect_profile_contexts(
            &profile(&temp.0),
            &MachineName::new("tron").expect("machine"),
            Instant::now(),
            DEFAULT_CONTEXT_MAX_AGE,
        )
        .expect("historical empty projects do not exhaust recent project cap");

        assert_eq!(collection.sessions.len(), 1);
        assert_eq!(collection.sessions[0].session_id.as_str(), "recent");
    }

    #[test]
    fn large_file_tail_is_bounded_and_partial_first_line_is_dropped() {
        let temp = TempDirectory::new();
        let filler = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "x".repeat(2_000) }
        })
        .to_string();
        let mut contents = String::new();
        for _ in 0..300 {
            contents.push_str(&filler);
            contents.push('\n');
        }
        contents.push_str(&assistant("tail-session", "claude-opus-4-7", 7, 11, 13));
        contents.push('\n');
        temp.write("projects/project/tail.jsonl", &contents);
        let collection = collect_profile_contexts(
            &profile(&temp.0),
            &MachineName::new("midnight").expect("machine"),
            Instant::now(),
            DEFAULT_CONTEXT_MAX_AGE,
        )
        .expect("collect");
        assert_eq!(collection.sessions[0].context_tokens, Some(31));
    }

    #[test]
    fn top_level_sessions_are_collected_but_subagents_and_symlinks_are_ignored() {
        let temp = TempDirectory::new();
        temp.write(
            "projects/project/main.jsonl",
            &format!("{}\n", assistant("main", "claude-opus-4-5", 1, 2, 3)),
        );
        temp.write(
            "projects/project/subagents/child.jsonl",
            &format!("{}\n", assistant("child", "claude-opus-4-5", 9, 9, 9)),
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let external = temp.write(
                "external.jsonl",
                &format!("{}\n", assistant("external", "claude-opus-4-5", 9, 9, 9)),
            );
            symlink(external, temp.0.join("projects/project/link.jsonl")).expect("symlink");
        }
        let collection = collect_profile_contexts(
            &profile(&temp.0),
            &MachineName::new("midnight").expect("machine"),
            Instant::now(),
            DEFAULT_CONTEXT_MAX_AGE,
        )
        .expect("collect");
        assert_eq!(
            collection
                .sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["main"]
        );
    }

    #[test]
    fn malformed_lines_do_not_override_healthy_assistant_usage() {
        let temp = TempDirectory::new();
        temp.write(
            "projects/project/session.jsonl",
            &format!(
                "not-json\n{}\n{}\n{{partial\n",
                assistant("session", "claude-opus-4-5", 1, 2, 3),
                serde_json::json!({
                    "type": "user",
                    "message": { "role": "user", "usage": { "input_tokens": 999_999 } }
                })
            ),
        );
        let collection = collect_profile_contexts(
            &profile(&temp.0),
            &MachineName::new("midnight").expect("machine"),
            Instant::now(),
            DEFAULT_CONTEXT_MAX_AGE,
        )
        .expect("collect");
        assert_eq!(collection.sessions[0].context_tokens, Some(6));
    }

    #[test]
    fn header_only_session_reports_zero_context_and_filename_fallback() {
        let temp = TempDirectory::new();
        temp.write(
            "projects/project/header.jsonl",
            "{\"type\":\"permission-mode\"}\n",
        );
        let collection = collect_profile_contexts(
            &profile(&temp.0),
            &MachineName::new("midnight").expect("machine"),
            Instant::now(),
            DEFAULT_CONTEXT_MAX_AGE,
        )
        .expect("collect");
        assert_eq!(collection.sessions[0].session_id.as_str(), "header");
        assert_eq!(collection.sessions[0].context_tokens, Some(0));
    }

    #[test]
    fn non_claude_profiles_and_absent_projects_are_gracefully_empty() {
        let temp = TempDirectory::new();
        let mut other = profile(&temp.0);
        other.vendor = Vendor::OpenaiCodex;
        assert!(
            collect_profile_contexts(
                &other,
                &MachineName::new("max").expect("machine"),
                Instant::now(),
                DEFAULT_CONTEXT_MAX_AGE
            )
            .expect("non-Claude")
            .sessions
            .is_empty()
        );
        assert!(
            collect_profile_contexts(
                &profile(&temp.0),
                &MachineName::new("max").expect("machine"),
                Instant::now(),
                DEFAULT_CONTEXT_MAX_AGE
            )
            .expect("absent projects")
            .sessions
            .is_empty()
        );
    }

    #[test]
    fn stale_sessions_are_omitted_and_future_poison_is_counted_per_file() {
        let temp = TempDirectory::new();
        let healthy = temp.write(
            "projects/project/healthy.jsonl",
            &format!("{}\n", assistant("healthy", "claude-opus-4-5", 1, 2, 3)),
        );
        let stale = temp.write(
            "projects/project/stale.jsonl",
            &format!("{}\n", assistant("stale", "claude-opus-4-5", 1, 2, 3)),
        );
        let future = temp.write(
            "projects/project/future.jsonl",
            &format!("{}\n", assistant("future", "claude-opus-4-5", 1, 2, 3)),
        );
        set_mtime(&healthy, 900_000);
        set_mtime(&stale, 100_000);
        set_mtime(&future, 1_300_001);

        let collection = collect_profile_contexts(
            &profile(&temp.0),
            &MachineName::new("midnight").expect("machine"),
            Instant::from_epoch_millis(1_000_000).expect("collected"),
            Duration::from_secs(10 * 60),
        )
        .expect("collect");

        assert_eq!(collection.failures, 1);
        assert_eq!(collection.sessions.len(), 1);
        assert_eq!(collection.sessions[0].session_id.as_str(), "healthy");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_ancestor_is_rejected_even_when_the_target_is_valid() {
        use std::os::unix::fs::symlink;

        let temp = TempDirectory::new();
        let real = temp.0.join("real-config");
        fs::create_dir_all(real.join("projects")).expect("real projects");
        let linked = temp.0.join("linked-config");
        symlink(&real, &linked).expect("config symlink");

        let error = collect_profile_contexts(
            &profile(&linked),
            &MachineName::new("midnight").expect("machine"),
            Instant::now(),
            DEFAULT_CONTEXT_MAX_AGE,
        )
        .expect_err("symlinked root");

        assert_eq!(error.kind(), PulseErrorKind::Configuration);
    }
}
