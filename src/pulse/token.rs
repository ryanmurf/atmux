//! Bounded local transcript tallying for Claude, Codex, and Antigravity.
//!
//! JSONL inputs are streamed line-by-line from no-follow regular file handles.
//! The result is an absolute per-day grain suitable for idempotent store upsert.

use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as _,
    io::{BufRead, BufReader, Read as _},
    path::{Path, PathBuf},
    time::{Duration, Instant as MonotonicInstant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    AccountId, AgentSettings, MachineName, Profile, ProfileName, PulseError, PulseErrorKind,
    PulseResult, SessionId, TokenGrain, TokenSource, Vendor,
    collect::{
        ScanLimits, ScannedFile, open_regular_bounded, scan_regular_files, scan_regular_files_since,
    },
};

const JSONL_SCAN: ScanLimits = ScanLimits {
    max_depth: 8,
    max_entries: 100_000,
    max_files: 2_048,
    max_file_bytes: 128 * 1024 * 1024,
    max_total_bytes: 512 * 1024 * 1024,
    max_duration: Duration::from_secs(12),
};
const MAX_LINES: usize = 2_000_000;
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_READ_BYTES: u64 = 512 * 1024 * 1024;
const MAX_READ_DURATION: Duration = Duration::from_secs(12);
const DEFAULT_MAX_ROWS: usize = 5_000;

/// Recent incremental collection or an explicit full-history backfill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TallyWindow {
    Recent { since_day: String },
    FullHistory,
}

/// Caller-controlled result bound. Files, bytes, lines, and time have stricter
/// internal hard caps regardless of this value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TallyOptions {
    pub window: TallyWindow,
    pub max_rows: usize,
}

impl TallyOptions {
    #[must_use]
    pub fn recent(since_day: impl Into<String>) -> Self {
        Self {
            window: TallyWindow::Recent {
                since_day: since_day.into(),
            },
            max_rows: DEFAULT_MAX_ROWS,
        }
    }

    #[must_use]
    pub const fn full_history() -> Self {
        Self {
            window: TallyWindow::FullHistory,
            max_rows: DEFAULT_MAX_ROWS,
        }
    }
}

/// Coarse compatibility projection derived from the fine grains.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoarseTokenRow {
    pub account_id: AccountId,
    pub profile: ProfileName,
    pub machine: MachineName,
    pub day: String,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

/// Work accounting returned with every local tally.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TallyStats {
    pub files_scanned: usize,
    pub lines_scanned: usize,
    pub bytes_read: u64,
    pub duplicate_events: usize,
    pub synthetic_events: usize,
}

/// Fine persisted grains plus their coarse derived view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTally {
    pub grains: Vec<TokenGrain>,
    pub coarse: Vec<CoarseTokenRow>,
    pub stats: TallyStats,
}

/// Stable keyset position for a full-history local token page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTallyCursor {
    pub day: String,
    pub session_id: String,
    pub model: String,
    pub settings_hash: String,
    pub source: TokenSource,
}

impl Ord for TokenTallyCursor {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (
            &self.day,
            &self.session_id,
            &self.model,
            &self.settings_hash,
            token_source_order(self.source),
        )
            .cmp(&(
                &other.day,
                &other.session_id,
                &other.model,
                &other.settings_hash,
                token_source_order(other.source),
            ))
    }
}

impl PartialOrd for TokenTallyCursor {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

const fn token_source_order(source: TokenSource) -> u8 {
    match source {
        TokenSource::Local => 0,
        TokenSource::Ingest => 1,
    }
}

impl TokenTallyCursor {
    #[must_use]
    pub fn from_grain(grain: &TokenGrain) -> Self {
        Self {
            day: grain.day.clone(),
            session_id: grain.session_id.as_str().to_owned(),
            model: grain.model.clone(),
            settings_hash: grain.settings_hash.clone(),
            source: grain.source,
        }
    }

    /// Validates the full local token natural-key cursor.
    ///
    /// # Errors
    ///
    /// Returns invalid input for malformed dimensions or a non-local source.
    pub fn validate(&self) -> PulseResult<()> {
        parse_day(&self.day)?;
        SessionId::new(self.session_id.clone())?;
        if self.model.is_empty()
            || self.model.len() > 256
            || self.model.chars().any(char::is_control)
            || self.settings_hash.len() != 64
            || !self
                .settings_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.source != TokenSource::Local
        {
            return Err(PulseError::invalid_input(
                "Pulse token tally cursor is invalid",
            ));
        }
        Ok(())
    }

    fn fine_key(&self) -> PulseResult<FineKey> {
        self.validate()?;
        Ok(FineKey {
            day: self.day.clone(),
            session: self.session_id.clone(),
            model: self.model.clone(),
            settings_hash: self.settings_hash.clone(),
        })
    }
}

/// Metadata witness for the bounded no-follow source scan used by a page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSourceGeneration(String);

impl TokenSourceGeneration {
    /// Parses a stored generation witness.
    ///
    /// # Errors
    ///
    /// Returns invalid input unless the value is lowercase SHA-256 hex.
    pub fn new(value: impl Into<String>) -> PulseResult<Self> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(PulseError::invalid_input(
                "Pulse token source generation is invalid",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Revalidates a deserialized or stored generation witness.
    ///
    /// # Errors
    ///
    /// Returns invalid input unless this remains lowercase SHA-256 hex.
    pub fn validate(&self) -> PulseResult<()> {
        Self::new(self.0.clone()).map(|_| ())
    }
}

/// One bounded keyset page from a stable local full-history source generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenTallyPage {
    pub tally: TokenTally,
    pub next_cursor: Option<TokenTallyCursor>,
    pub complete: bool,
    pub source_generation: TokenSourceGeneration,
}

/// Tallies one profile from its local, externally configured storage.
///
/// # Errors
///
/// Returns a bounded storage/configuration error for unsafe paths, excessive
/// work, arithmetic overflow, invalid dates, or malformed retained dimensions.
pub fn tally_profile(
    profile: &Profile,
    machine: MachineName,
    options: &TallyOptions,
) -> PulseResult<TokenTally> {
    tally_profile_inner(profile, machine, options, None, false).map(|(tally, _)| tally)
}

/// Tallies one bounded full-history keyset page without materializing later rows.
///
/// The source generation is checked before and after the scan. A metadata
/// change fails the page so callers never advance a durable cursor across a
/// visibly changing source.
///
/// # Errors
///
/// Returns a bounded source/configuration error, or conflict when the source
/// metadata changes during the page scan.
pub fn tally_profile_page(
    profile: &Profile,
    machine: &MachineName,
    after: Option<&TokenTallyCursor>,
    max_rows: usize,
) -> PulseResult<TokenTallyPage> {
    let options = TallyOptions {
        window: TallyWindow::FullHistory,
        max_rows,
    };
    validate_options(&options)?;
    let before = token_source_generation(profile, machine)?;
    let after_key = after.map(TokenTallyCursor::fine_key).transpose()?;
    let (tally, truncated) =
        tally_profile_inner(profile, machine.clone(), &options, after_key, true)?;
    let after_generation = token_source_generation(profile, machine)?;
    if before != after_generation {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "Pulse token source changed during full-history paging",
        ));
    }
    let next_cursor = tally.grains.last().map(TokenTallyCursor::from_grain);
    Ok(TokenTallyPage {
        tally,
        next_cursor,
        complete: !truncated,
        source_generation: before,
    })
}

fn tally_profile_inner(
    profile: &Profile,
    machine: MachineName,
    options: &TallyOptions,
    after: Option<FineKey>,
    paged: bool,
) -> PulseResult<(TokenTally, bool)> {
    validate_options(options)?;
    if matches!(profile.vendor, Vendor::XaiGrok | Vendor::Gemini) {
        return Ok((empty_tally(), false));
    }
    let config_dir = profile.config_dir.as_deref().ok_or_else(|| {
        PulseError::configuration("token tally requires an explicit profile config_dir")
    })?;
    if !config_dir.is_absolute() {
        return Err(PulseError::configuration(
            "token tally config_dir must be absolute",
        ));
    }
    let day_floor = day_floor(&options.window)?;
    let mtime_floor = mtime_floor(day_floor.as_deref())?;
    let mut tally = FineTally::new(profile, machine, options.max_rows, after, paged);
    let mut budget = ReadBudget::default();
    match profile.vendor {
        Vendor::OpenaiCodex => {
            tally_codex(
                config_dir,
                mtime_floor,
                day_floor.as_deref(),
                &mut tally,
                &mut budget,
            )?;
        }
        Vendor::Antigravity => {
            tally_antigravity(
                config_dir,
                mtime_floor,
                day_floor.as_deref(),
                &mut tally,
                &mut budget,
            )?;
        }
        Vendor::AnthropicOauth | Vendor::DeepseekBalance => {
            tally_claude(
                config_dir,
                mtime_floor,
                day_floor.as_deref(),
                &mut tally,
                &mut budget,
            )?;
        }
        Vendor::XaiGrok | Vendor::Gemini => return Ok((empty_tally(), false)),
    }
    let stats = TallyStats {
        duplicate_events: tally.duplicate_events,
        synthetic_events: tally.synthetic_events,
        ..budget.stats
    };
    let (grains, truncated) = tally.finish()?;
    let coarse = coarse_rows(&grains)?;
    Ok((
        TokenTally {
            grains,
            coarse,
            stats,
        },
        truncated,
    ))
}

fn empty_tally() -> TokenTally {
    TokenTally {
        grains: Vec::new(),
        coarse: Vec::new(),
        stats: TallyStats::default(),
    }
}

fn validate_options(options: &TallyOptions) -> PulseResult<()> {
    if options.max_rows == 0 || options.max_rows > DEFAULT_MAX_ROWS {
        return Err(PulseError::invalid_input(format!(
            "token tally max_rows must be between 1 and {DEFAULT_MAX_ROWS}"
        )));
    }
    if let TallyWindow::Recent { since_day } = &options.window {
        parse_day(since_day)?;
    }
    Ok(())
}

fn day_floor(window: &TallyWindow) -> PulseResult<Option<String>> {
    match window {
        TallyWindow::Recent { since_day } => {
            parse_day(since_day)?;
            Ok(Some(since_day.clone()))
        }
        TallyWindow::FullHistory => Ok(None),
    }
}

fn parse_day(day: &str) -> PulseResult<jiff::civil::Date> {
    day.parse::<jiff::civil::Date>()
        .map_err(|error| PulseError::invalid_input(format!("invalid tally day: {error}")))
}

fn mtime_floor(day: Option<&str>) -> PulseResult<Option<i64>> {
    day.map(|day| {
        let midnight = super::Instant::from_iso8601(&format!("{day}T00:00:00Z"))?;
        midnight
            .epoch_millis()
            .checked_sub(24 * 60 * 60 * 1_000)
            .ok_or_else(|| PulseError::invalid_input("tally mtime floor overflowed"))
    })
    .transpose()
}

/// Computes a bounded metadata witness for one profile/machine source scope.
///
/// # Errors
///
/// Returns a safe configuration/storage error for an unsafe or excessive
/// local source tree.
pub fn token_source_generation(
    profile: &Profile,
    machine: &MachineName,
) -> PulseResult<TokenSourceGeneration> {
    let mut hasher = Sha256::new();
    hash_generation_field(&mut hasher, b"atmux-pulse-token-page-v1");
    hash_generation_field(&mut hasher, &profile.account_id.get().to_be_bytes());
    hash_generation_field(&mut hasher, profile.name.as_str().as_bytes());
    hash_generation_field(&mut hasher, machine.as_str().as_bytes());
    hash_generation_field(
        &mut hasher,
        &serde_json::to_vec(&profile.vendor).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Internal,
                "Pulse token source vendor could not be encoded",
            )
        })?,
    );

    let mut files = source_generation_files(profile)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    if let Some(config_dir) = &profile.config_dir {
        hash_generation_field(&mut hasher, config_dir.to_string_lossy().as_bytes());
        for file in files {
            let relative = file.path.strip_prefix(config_dir).unwrap_or(&file.path);
            hash_generation_file(&mut hasher, relative, &file);
        }
    }
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    TokenSourceGeneration::new(encoded)
}

fn source_generation_files(profile: &Profile) -> PulseResult<Vec<ScannedFile>> {
    if matches!(profile.vendor, Vendor::XaiGrok | Vendor::Gemini) {
        return Ok(Vec::new());
    }
    let config_dir = profile.config_dir.as_deref().ok_or_else(|| {
        PulseError::configuration("token tally requires an explicit profile config_dir")
    })?;
    if !config_dir.is_absolute() {
        return Err(PulseError::configuration(
            "token tally config_dir must be absolute",
        ));
    }
    let (root, extension, sqlite_sidecars) = match profile.vendor {
        Vendor::OpenaiCodex => (config_dir.join("sessions"), "jsonl", false),
        Vendor::Antigravity => (config_dir.join("conversations"), "db", true),
        Vendor::AnthropicOauth | Vendor::DeepseekBalance => {
            (config_dir.join("projects"), "jsonl", false)
        }
        Vendor::XaiGrok | Vendor::Gemini => return Ok(Vec::new()),
    };
    match scan_regular_files(&root, JSONL_SCAN, |path| {
        let expected_extension = path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(extension));
        let sqlite_sidecar = sqlite_sidecars
            && path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.ends_with(".db-wal") || value.ends_with(".db-shm"));
        expected_extension || sqlite_sidecar
    }) {
        Ok(files) => Ok(files),
        Err(error) if error.kind() == PulseErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn hash_generation_file(hasher: &mut Sha256, relative: &Path, file: &ScannedFile) {
    hash_generation_field(hasher, relative.to_string_lossy().as_bytes());
    hash_generation_field(hasher, &file.size.to_be_bytes());
    if !relative
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.ends_with(".db-shm"))
    {
        hash_generation_field(hasher, &file.modified_ms.to_be_bytes());
    }
}

fn hash_generation_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FineKey {
    day: String,
    session: String,
    model: String,
    settings_hash: String,
}

#[derive(Clone, Copy, Debug, Default)]
struct TokenDelta {
    input: u64,
    output: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    cache_read: u64,
}

struct FineTally {
    account_id: AccountId,
    profile: ProfileName,
    machine: MachineName,
    rows: BTreeMap<FineKey, TokenGrain>,
    seen: HashSet<String>,
    max_rows: usize,
    after: Option<FineKey>,
    paged: bool,
    truncated: bool,
    duplicate_events: usize,
    synthetic_events: usize,
}

impl FineTally {
    fn new(
        profile: &Profile,
        machine: MachineName,
        max_rows: usize,
        after: Option<FineKey>,
        paged: bool,
    ) -> Self {
        Self {
            account_id: profile.account_id,
            profile: profile.name.clone(),
            machine,
            rows: BTreeMap::new(),
            seen: HashSet::new(),
            max_rows,
            after,
            paged,
            truncated: false,
            duplicate_events: 0,
            synthetic_events: 0,
        }
    }

    fn add(
        &mut self,
        dimensions: EventDimensions,
        delta: TokenDelta,
        dedupe_key: Option<String>,
    ) -> PulseResult<()> {
        if let Some(key) = dedupe_key
            && !self.seen.insert(key)
        {
            self.duplicate_events = self.duplicate_events.saturating_add(1);
            return Ok(());
        }
        let settings_hash = dimensions.settings.sha256()?;
        let session_id = SessionId::new(dimensions.session.clone())?;
        let key = FineKey {
            day: dimensions.day.clone(),
            session: dimensions.session.clone(),
            model: dimensions.model.clone(),
            settings_hash: settings_hash.clone(),
        };
        if self.after.as_ref().is_some_and(|after| key <= *after) {
            return Ok(());
        }
        if !self.rows.contains_key(&key) && self.rows.len() >= self.max_rows {
            if !self.paged {
                return Err(PulseError::new(
                    PulseErrorKind::Storage,
                    "token tally exceeded its output row bound",
                ));
            }
            self.truncated = true;
            let retain = self
                .rows
                .last_key_value()
                .is_some_and(|(last, _)| key < *last);
            if !retain {
                return Ok(());
            }
            self.rows.pop_last();
        }
        let row = self.rows.entry(key).or_insert_with(|| TokenGrain {
            account_id: self.account_id,
            profile: self.profile.clone(),
            machine: self.machine.clone(),
            session_id,
            model: dimensions.model,
            settings: dimensions.settings,
            settings_hash,
            day: dimensions.day,
            tokens_in: 0,
            tokens_out: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
            source: TokenSource::Local,
        });
        row.tokens_in = checked_add(row.tokens_in, delta.input)?;
        row.tokens_out = checked_add(row.tokens_out, delta.output)?;
        row.cache_write_5m = checked_add(row.cache_write_5m, delta.cache_write_5m)?;
        row.cache_write_1h = checked_add(row.cache_write_1h, delta.cache_write_1h)?;
        row.cache_read = checked_add(row.cache_read, delta.cache_read)?;
        Ok(())
    }

    fn finish(self) -> PulseResult<(Vec<TokenGrain>, bool)> {
        let rows = self
            .rows
            .into_values()
            .map(|grain| {
                grain.validate()?;
                Ok(grain)
            })
            .collect::<PulseResult<Vec<_>>>()?;
        Ok((rows, self.truncated))
    }
}

fn checked_add(left: u64, right: u64) -> PulseResult<u64> {
    left.checked_add(right)
        .ok_or_else(|| PulseError::invalid_input("token tally overflowed"))
}

struct EventDimensions {
    day: String,
    session: String,
    model: String,
    settings: AgentSettings,
}

#[derive(Default)]
struct ReadBudget {
    stats: TallyStats,
    started: Option<MonotonicInstant>,
}

impl ReadBudget {
    fn begin(&mut self) {
        self.started.get_or_insert_with(MonotonicInstant::now);
    }

    fn observe_line(&mut self, bytes: usize) -> PulseResult<()> {
        self.stats.lines_scanned = self.stats.lines_scanned.saturating_add(1);
        self.stats.bytes_read = self
            .stats
            .bytes_read
            .checked_add(u64::try_from(bytes).unwrap_or(u64::MAX))
            .ok_or_else(|| {
                PulseError::new(PulseErrorKind::Storage, "token read size overflowed")
            })?;
        if self.stats.lines_scanned > MAX_LINES || self.stats.bytes_read > MAX_READ_BYTES {
            return Err(PulseError::new(
                PulseErrorKind::Storage,
                "token transcript work bound was exceeded",
            ));
        }
        if self
            .started
            .is_some_and(|started| started.elapsed() > MAX_READ_DURATION)
        {
            return Err(PulseError::new(
                PulseErrorKind::Storage,
                "token transcript time bound was exceeded",
            ));
        }
        Ok(())
    }
}

fn scan_jsonl(root: &Path, mtime_floor: Option<i64>) -> PulseResult<Vec<PathBuf>> {
    let mut files = match scan_regular_files_since(root, JSONL_SCAN, mtime_floor, |path| {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
    }) {
        Ok(files) => files,
        Err(error) if error.kind() == PulseErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files.into_iter().map(|file| file.path).collect())
}

fn visit_lines(
    files: &[PathBuf],
    budget: &mut ReadBudget,
    mut visit: impl FnMut(&Path, &[u8]) -> PulseResult<()>,
) -> PulseResult<()> {
    budget.begin();
    for path in files {
        let file = open_regular_bounded(path, JSONL_SCAN.max_file_bytes).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Storage,
                "token transcript was unavailable or unsafe",
            )
        })?;
        budget.stats.files_scanned = budget.stats.files_scanned.saturating_add(1);
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            let mut bounded = reader
                .by_ref()
                .take(u64::try_from(MAX_LINE_BYTES + 1).unwrap_or(u64::MAX));
            let bytes = bounded.read_until(b'\n', &mut line).map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Storage,
                    "token transcript could not be read",
                )
            })?;
            if bytes == 0 {
                break;
            }
            if bytes > MAX_LINE_BYTES {
                return Err(PulseError::new(
                    PulseErrorKind::Storage,
                    "token transcript line exceeded its size bound",
                ));
            }
            budget.observe_line(bytes)?;
            visit(path, &line)?;
        }
    }
    Ok(())
}

fn tally_claude(
    config_dir: &Path,
    mtime_floor: Option<i64>,
    day_floor: Option<&str>,
    tally: &mut FineTally,
    budget: &mut ReadBudget,
) -> PulseResult<()> {
    let files = scan_jsonl(&config_dir.join("projects"), mtime_floor)?;
    visit_lines(&files, budget, |path, line| {
        if !line.windows(5).any(|window| window == b"usage") {
            return Ok(());
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return Ok(());
        };
        let Some(message) = value.get("message").and_then(Value::as_object) else {
            return Ok(());
        };
        let Some(usage) = message.get("usage") else {
            return Ok(());
        };
        let assistant = message.get("role").and_then(Value::as_str) == Some("assistant")
            || value.get("type").and_then(Value::as_str) == Some("assistant");
        if !assistant {
            return Ok(());
        }
        let Some(day) = event_day(value.get("timestamp")) else {
            return Ok(());
        };
        if day_floor.is_some_and(|floor| day.as_str() < floor) {
            return Ok(());
        }
        let model = message
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if model == "<synthetic>" {
            tally.synthetic_events = tally.synthetic_events.saturating_add(1);
            return Ok(());
        }
        let session = value
            .get("sessionId")
            .and_then(Value::as_str)
            .map_or_else(|| file_session(path), str::to_owned);
        let settings = AgentSettings {
            service_tier: usage
                .get("service_tier")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ..AgentSettings::default()
        };
        let (short_cache_write, hourly_cache_write) = split_cache_creation(usage);
        let message_id = message.get("id").and_then(Value::as_str).unwrap_or("");
        let request_id = value.get("requestId").and_then(Value::as_str).unwrap_or("");
        let dedupe = (!message_id.is_empty() || !request_id.is_empty())
            .then(|| format!("claude:{message_id}:{request_id}"));
        tally.add(
            EventDimensions {
                day,
                session,
                model: model.to_owned(),
                settings,
            },
            TokenDelta {
                input: token_value(usage.get("input_tokens")),
                output: token_value(usage.get("output_tokens")),
                cache_write_5m: short_cache_write,
                cache_write_1h: hourly_cache_write,
                cache_read: token_value(usage.get("cache_read_input_tokens")),
            },
            dedupe,
        )
    })
}

fn split_cache_creation(usage: &Value) -> (u64, u64) {
    let cache = usage.get("cache_creation");
    let five = token_value(cache.and_then(|value| value.get("ephemeral_5m_input_tokens")));
    let hour = token_value(cache.and_then(|value| value.get("ephemeral_1h_input_tokens")));
    if five > 0 || hour > 0 {
        (five, hour)
    } else {
        (token_value(usage.get("cache_creation_input_tokens")), 0)
    }
}

fn tally_codex(
    config_dir: &Path,
    mtime_floor: Option<i64>,
    day_floor: Option<&str>,
    tally: &mut FineTally,
    budget: &mut ReadBudget,
) -> PulseResult<()> {
    let files = scan_jsonl(&config_dir.join("sessions"), mtime_floor)?;
    budget.begin();
    for path in files {
        let mut model = None::<String>;
        let mut effort = None::<String>;
        let mut service_tier = None::<String>;
        let mut event_index = 0_usize;
        visit_lines(std::slice::from_ref(&path), budget, |_, line| {
            let Ok(value) = serde_json::from_slice::<Value>(line) else {
                return Ok(());
            };
            let payload = value.get("payload").unwrap_or(&value);
            let turn_context = value.get("type").and_then(Value::as_str) == Some("turn_context")
                || payload.get("type").and_then(Value::as_str) == Some("turn_context");
            if turn_context {
                if let Some(found) = codex_model(payload) {
                    model = Some(found.to_owned());
                }
                effort = first_string(
                    payload,
                    &[
                        &["effort"],
                        &["reasoning_effort"],
                        &["collaboration_mode", "settings", "reasoning_effort"],
                        &["collaboration_mode", "settings", "effort"],
                    ],
                )
                .map(str::to_owned)
                .or(effort.take());
                service_tier = first_string(
                    payload,
                    &[
                        &["service_tier"],
                        &["collaboration_mode", "settings", "service_tier"],
                    ],
                )
                .map(str::to_owned)
                .or(service_tier.take());
                return Ok(());
            }
            let info = payload.get("info").unwrap_or(payload);
            let usage = info
                .get("last_token_usage")
                .or_else(|| info.get("token_usage"));
            let Some(usage) = usage else {
                return Ok(());
            };
            let Some(day) = event_day(value.get("timestamp").or_else(|| payload.get("timestamp")))
            else {
                return Ok(());
            };
            if day_floor.is_some_and(|floor| day.as_str() < floor) {
                return Ok(());
            }
            let input = token_value(usage.get("input_tokens"));
            let cached = token_value(
                usage
                    .get("cached_input_tokens")
                    .or_else(|| usage.get("cache_read_input_tokens")),
            );
            let output = checked_add(
                token_value(usage.get("output_tokens")),
                token_value(usage.get("reasoning_output_tokens")),
            )?;
            let settings = AgentSettings {
                service_tier: service_tier.clone(),
                effort: effort.clone(),
                ..AgentSettings::default()
            };
            let dedupe = format!("codex:{}:{event_index}", path.display());
            event_index = event_index.saturating_add(1);
            tally.add(
                EventDimensions {
                    day,
                    session: file_session(&path),
                    model: model.clone().unwrap_or_else(|| "gpt-5".to_owned()),
                    settings,
                },
                TokenDelta {
                    input: input.saturating_sub(cached),
                    output,
                    cache_write_5m: 0,
                    cache_write_1h: 0,
                    cache_read: cached,
                },
                Some(dedupe),
            )
        })?;
    }
    Ok(())
}

fn codex_model(payload: &Value) -> Option<&str> {
    payload.get("model").and_then(Value::as_str).or_else(|| {
        nested(payload, &["collaboration_mode", "settings", "model"]).and_then(Value::as_str)
    })
}

fn first_string<'a>(value: &'a Value, paths: &[&[&str]]) -> Option<&'a str> {
    paths
        .iter()
        .find_map(|path| nested(value, path).and_then(Value::as_str))
}

fn nested<'a>(mut value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    for key in path {
        value = value.get(*key)?;
    }
    Some(value)
}

fn tally_antigravity(
    config_dir: &Path,
    mtime_floor: Option<i64>,
    day_floor: Option<&str>,
    tally: &mut FineTally,
    budget: &mut ReadBudget,
) -> PulseResult<()> {
    let conversations =
        super::collect::antigravity::collect_conversations(config_dir, mtime_floor)?;
    budget.stats.files_scanned = conversations.len();
    for conversation in conversations {
        if day_floor.is_some_and(|floor| conversation.day.as_str() < floor) {
            continue;
        }
        tally.add(
            EventDimensions {
                day: conversation.day,
                session: conversation.session_id.clone(),
                model: conversation.model,
                settings: AgentSettings::default(),
            },
            TokenDelta {
                input: conversation.usage.prompt,
                output: checked_add(conversation.usage.output, conversation.usage.thinking)?,
                ..TokenDelta::default()
            },
            Some(format!("antigravity:{}", conversation.session_id)),
        )?;
    }
    Ok(())
}

fn event_day(timestamp: Option<&Value>) -> Option<String> {
    let timestamp = timestamp?.as_str()?;
    let instant = super::Instant::from_iso8601(timestamp).ok()?;
    instant.to_iso8601().get(0..10).map(str::to_owned)
}

fn token_value(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

fn file_session(path: &Path) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown-session")
        .to_owned()
}

fn coarse_rows(grains: &[TokenGrain]) -> PulseResult<Vec<CoarseTokenRow>> {
    let mut rows = BTreeMap::<(String, String), CoarseTokenRow>::new();
    for grain in grains {
        let key = (grain.day.clone(), grain.model.clone());
        let row = rows.entry(key).or_insert_with(|| CoarseTokenRow {
            account_id: grain.account_id,
            profile: grain.profile.clone(),
            machine: grain.machine.clone(),
            day: grain.day.clone(),
            model: grain.model.clone(),
            tokens_in: 0,
            tokens_out: 0,
            cache_creation: 0,
            cache_read: 0,
        });
        row.tokens_in = checked_add(row.tokens_in, grain.tokens_in)?;
        row.tokens_out = checked_add(row.tokens_out, grain.tokens_out)?;
        row.cache_creation = checked_add(
            row.cache_creation,
            checked_add(grain.cache_write_5m, grain.cache_write_1h)?,
        )?;
        row.cache_read = checked_add(row.cache_read, grain.cache_read)?;
    }
    Ok(rows.into_values().collect())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::UNIX_EPOCH,
    };

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "atmux-pulse-token-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp directory");
            Self(path)
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().expect("parent")).expect("create fixture parent");
            fs::write(&path, contents).expect("write fixture");
            path
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn profile(directory: &Path, vendor: Vendor) -> Profile {
        Profile {
            account_id: AccountId::new(1).expect("account"),
            name: ProfileName::new("profile").expect("profile"),
            vendor,
            config_dir: Some(directory.to_path_buf()),
            poll_interval_minutes: 15,
            monthly_budget_usd: None,
            api_key_env: None,
            api_key_file: None,
            refresh: super::super::RefreshPolicy::Never,
            hidden: false,
            origin: crate::pulse::ProfileOrigin::Local,
        }
    }

    #[test]
    fn claude_tally_dedupes_synthetic_and_splits_cache() {
        let temp = TempDirectory::new();
        let first = r#"{"type":"assistant","sessionId":"s1","requestId":"r1","timestamp":"2026-08-08T01:00:00Z","message":{"role":"assistant","id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":100,"output_tokens":20,"cache_creation_input_tokens":50,"cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":20},"cache_read_input_tokens":200,"service_tier":"standard"}}}"#;
        let synthetic = r#"{"type":"assistant","timestamp":"2026-08-08T01:01:00Z","message":{"role":"assistant","model":"<synthetic>","usage":{"input_tokens":999}}}"#;
        let user = r#"{"type":"user","timestamp":"2026-08-08T01:02:00Z","message":{"role":"user","model":"claude-opus-4-8","usage":{"input_tokens":999}}}"#;
        temp.write(
            "projects/project/session.jsonl",
            &format!("{first}\n{first}\n{synthetic}\n{user}\n"),
        );
        let result = tally_profile(
            &profile(&temp.0, Vendor::AnthropicOauth),
            MachineName::new("midnight").expect("machine"),
            &TallyOptions::full_history(),
        )
        .expect("tally");
        assert_eq!(result.grains.len(), 1);
        let row = &result.grains[0];
        assert_eq!(row.tokens_in, 100);
        assert_eq!(row.tokens_out, 20);
        assert_eq!(row.cache_write_5m, 30);
        assert_eq!(row.cache_write_1h, 20);
        assert_eq!(row.cache_read, 200);
        assert_eq!(row.settings.service_tier.as_deref(), Some("standard"));
        assert_eq!(result.stats.duplicate_events, 1);
        assert_eq!(result.stats.synthetic_events, 1);
    }

    #[test]
    fn codex_tally_uses_delta_splits_cache_and_captures_effort() {
        let temp = TempDirectory::new();
        let lines = [
            r#"{"type":"turn_context","payload":{"type":"turn_context","model":"gpt-5.5","effort":"high"}}"#,
            r#"{"timestamp":"2026-08-08T02:00:00Z","payload":{"info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":200,"reasoning_output_tokens":100}}}}"#,
            r#"{"timestamp":"2026-08-08T02:01:00Z","payload":{"info":{"total_token_usage":{"input_tokens":999999}}}}"#,
        ]
        .join("\n");
        temp.write("sessions/2026/08/rollout-x.jsonl", &lines);
        let result = tally_profile(
            &profile(&temp.0, Vendor::OpenaiCodex),
            MachineName::new("max").expect("machine"),
            &TallyOptions::full_history(),
        )
        .expect("tally");
        assert_eq!(result.grains.len(), 1);
        let row = &result.grains[0];
        assert_eq!(row.session_id.as_str(), "rollout-x");
        assert_eq!(row.tokens_in, 200);
        assert_eq!(row.cache_read, 800);
        assert_eq!(row.tokens_out, 300);
        assert_eq!(row.settings.effort.as_deref(), Some("high"));
    }

    #[test]
    fn recent_filters_days_while_full_history_includes_them() {
        let temp = TempDirectory::new();
        let old = r#"{"type":"assistant","sessionId":"s","requestId":"old","timestamp":"2026-08-01T01:00:00Z","message":{"role":"assistant","id":"old","model":"claude-opus-4-8","usage":{"input_tokens":1}}}"#;
        let new = r#"{"type":"assistant","sessionId":"s","requestId":"new","timestamp":"2026-08-08T01:00:00Z","message":{"role":"assistant","id":"new","model":"claude-opus-4-8","usage":{"input_tokens":2}}}"#;
        temp.write("projects/project/session.jsonl", &format!("{old}\n{new}\n"));
        let profile = profile(&temp.0, Vendor::AnthropicOauth);
        let recent = tally_profile(
            &profile,
            MachineName::new("max").expect("machine"),
            &TallyOptions::recent("2026-08-08"),
        )
        .expect("recent");
        let full = tally_profile(
            &profile,
            MachineName::new("max").expect("machine"),
            &TallyOptions::full_history(),
        )
        .expect("full");
        assert_eq!(recent.grains.len(), 1);
        assert_eq!(recent.grains[0].day, "2026-08-08");
        assert_eq!(full.grains.len(), 2);
    }

    #[test]
    fn recent_scan_filters_old_files_before_file_and_byte_work_caps() {
        let temp = TempDirectory::new();
        let old_time = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        for index in 0..=JSONL_SCAN.max_files {
            let path = temp.write(&format!("projects/project/old-{index}.jsonl"), "{}\n");
            let file = fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("open old fixture");
            file.set_times(fs::FileTimes::new().set_modified(old_time))
                .expect("age old fixture");
        }
        let oversized = temp.write("projects/project/old-oversized.jsonl", "");
        let oversized_file = fs::OpenOptions::new()
            .write(true)
            .open(oversized)
            .expect("open oversized old fixture");
        oversized_file
            .set_len(JSONL_SCAN.max_file_bytes + 1)
            .expect("make sparse oversized fixture");
        oversized_file
            .set_times(fs::FileTimes::new().set_modified(old_time))
            .expect("age oversized old fixture");
        let recent = r#"{"type":"assistant","sessionId":"recent","requestId":"recent","timestamp":"2026-08-09T01:00:00Z","message":{"role":"assistant","id":"recent","model":"claude-opus-4-8","usage":{"input_tokens":7}}}"#;
        temp.write("projects/project/recent.jsonl", recent);

        let tally = tally_profile(
            &profile(&temp.0, Vendor::AnthropicOauth),
            MachineName::new("tron").expect("machine"),
            &TallyOptions::recent("2026-08-08"),
        )
        .expect("recent scan ignores old files before work caps");

        assert_eq!(tally.grains.len(), 1);
        assert_eq!(tally.grains[0].session_id.as_str(), "recent");
        assert_eq!(tally.stats.files_scanned, 1);
    }

    #[test]
    fn full_history_pages_use_stable_complete_natural_key_cursors() {
        let temp = TempDirectory::new();
        let rows = (1..=3)
            .map(|index| {
                format!(
                    r#"{{"type":"assistant","sessionId":"s{index}","requestId":"r{index}","timestamp":"2026-08-0{index}T01:00:00Z","message":{{"role":"assistant","id":"m{index}","model":"claude-opus-4-8","usage":{{"input_tokens":{index}}}}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        temp.write("projects/project/session.jsonl", &rows);
        let profile = profile(&temp.0, Vendor::AnthropicOauth);
        let machine = MachineName::new("max").expect("machine");
        let first = tally_profile_page(&profile, &machine, None, 2).expect("first page");
        assert_eq!(first.tally.grains.len(), 2);
        assert!(!first.complete);
        let cursor = first.next_cursor.clone().expect("first cursor");
        assert_eq!(cursor.source, TokenSource::Local);
        let second = tally_profile_page(&profile, &machine, Some(&cursor), 2).expect("second page");
        assert_eq!(second.tally.grains.len(), 1);
        assert!(second.complete);
        assert_eq!(first.source_generation, second.source_generation);

        let mut identities = first
            .tally
            .grains
            .iter()
            .chain(&second.tally.grains)
            .map(|grain| {
                (
                    grain.day.clone(),
                    grain.session_id.as_str().to_owned(),
                    grain.model.clone(),
                    grain.settings_hash.clone(),
                    grain.source,
                )
            })
            .collect::<Vec<_>>();
        identities.sort_by(|left, right| left.0.cmp(&right.0));
        identities.dedup();
        assert_eq!(identities.len(), 3);
    }

    #[test]
    fn full_history_paging_dedupes_events_seen_before_the_cursor() {
        let temp = TempDirectory::new();
        let first = r#"{"type":"assistant","sessionId":"s1","requestId":"same","timestamp":"2026-08-01T01:00:00Z","message":{"role":"assistant","id":"same","model":"claude-opus-4-8","usage":{"input_tokens":1}}}"#;
        let second = r#"{"type":"assistant","sessionId":"s2","requestId":"unique","timestamp":"2026-08-02T01:00:00Z","message":{"role":"assistant","id":"unique","model":"claude-opus-4-8","usage":{"input_tokens":2}}}"#;
        let duplicate = r#"{"type":"assistant","sessionId":"s3","requestId":"same","timestamp":"2026-08-03T01:00:00Z","message":{"role":"assistant","id":"same","model":"claude-opus-4-8","usage":{"input_tokens":99}}}"#;
        temp.write(
            "projects/project/session.jsonl",
            &format!("{first}\n{second}\n{duplicate}\n"),
        );
        let profile = profile(&temp.0, Vendor::AnthropicOauth);
        let machine = MachineName::new("max").expect("machine");
        let one_shot = tally_profile(&profile, machine.clone(), &TallyOptions::full_history())
            .expect("one-shot tally");
        let first_page = tally_profile_page(&profile, &machine, None, 1).expect("first page");
        let second_page =
            tally_profile_page(&profile, &machine, first_page.next_cursor.as_ref(), 1)
                .expect("second page");
        assert!(!first_page.complete);
        assert!(second_page.complete);
        assert_eq!(first_page.tally.grains.len(), 1);
        assert_eq!(second_page.tally.grains.len(), 1);
        let paged_total = first_page.tally.grains[0]
            .tokens_in
            .saturating_add(second_page.tally.grains[0].tokens_in);
        assert_eq!(paged_total, 3);
        assert_eq!(
            paged_total,
            one_shot
                .grains
                .iter()
                .map(|grain| grain.tokens_in)
                .sum::<u64>()
        );
    }

    #[test]
    fn source_generation_detects_bounded_metadata_and_scope_changes() {
        let temp = TempDirectory::new();
        let line = r#"{"type":"assistant","sessionId":"s1","requestId":"r1","timestamp":"2026-08-08T01:00:00Z","message":{"role":"assistant","id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":1}}}"#;
        temp.write("projects/project/session.jsonl", line);
        let profile = profile(&temp.0, Vendor::AnthropicOauth);
        let machine = MachineName::new("max").expect("machine");
        let first = tally_profile_page(&profile, &machine, None, 2).expect("first page");

        temp.write(
            "projects/project/session.jsonl",
            &format!("{line}\n{line}\n"),
        );
        let changed = tally_profile_page(&profile, &machine, None, 2).expect("changed source page");
        assert_ne!(first.source_generation, changed.source_generation);

        let other_machine = tally_profile_page(
            &profile,
            &MachineName::new("other").expect("other machine"),
            None,
            2,
        )
        .expect("other scoped page");
        assert_ne!(changed.source_generation, other_machine.source_generation);

        let mut invalid = changed.next_cursor.expect("cursor");
        invalid.source = TokenSource::Ingest;
        assert!(tally_profile_page(&profile, &machine, Some(&invalid), 2).is_err());
    }

    #[test]
    fn antigravity_source_generation_detects_wal_only_changes() {
        let temp = TempDirectory::new();
        let path = temp.0.join("conversations/conversation.db");
        fs::create_dir_all(path.parent().expect("parent")).expect("conversation parent");
        let connection = rusqlite::Connection::open(&path).expect("conversation database");
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .expect("enable WAL");
        connection
            .pragma_update(None, "wal_autocheckpoint", 0_i64)
            .expect("disable auto-checkpoint");
        connection
            .execute_batch("CREATE TABLE steps (step_payload BLOB NOT NULL) STRICT;")
            .expect("conversation schema");
        let payload = vec![0x10, 100, 0x48, 10, 0x50, 20, 0x18, 30];
        connection
            .execute("INSERT INTO steps (step_payload) VALUES (?1)", [&payload])
            .expect("first WAL row");

        let profile = profile(&temp.0, Vendor::Antigravity);
        let machine = MachineName::new("max").expect("machine");
        let first = tally_profile_page(&profile, &machine, None, 2).expect("first WAL page");
        let main_before = fs::metadata(&path).expect("main DB metadata");
        let wal_path = PathBuf::from(format!("{}-wal", path.display()));
        let wal_before = fs::metadata(&wal_path).expect("WAL metadata").len();

        connection
            .execute("INSERT INTO steps (step_payload) VALUES (?1)", [&payload])
            .expect("second WAL row");
        let main_after = fs::metadata(&path).expect("unchanged main DB metadata");
        assert_eq!(main_before.len(), main_after.len());
        assert_eq!(
            main_before.modified().expect("main modified time"),
            main_after.modified().expect("main modified time")
        );
        assert!(fs::metadata(&wal_path).expect("changed WAL metadata").len() > wal_before);

        let changed = tally_profile_page(&profile, &machine, None, 2).expect("changed WAL page");
        assert_ne!(first.source_generation, changed.source_generation);
    }

    #[test]
    fn antigravity_tally_projects_bounded_conversation_usage() {
        let temp = TempDirectory::new();
        let path = temp.0.join("conversations/conversation.db");
        fs::create_dir_all(path.parent().expect("parent")).expect("conversation parent");
        {
            let connection = rusqlite::Connection::open(&path).expect("conversation database");
            connection
                .execute_batch("CREATE TABLE steps (step_payload BLOB NOT NULL) STRICT;")
                .expect("conversation schema");
            // field2=prompt(100), field9=thinking(10), field10=output(20),
            // field3=checksum(output+thinking=30).
            connection
                .execute(
                    "INSERT INTO steps (step_payload) VALUES (?1)",
                    [vec![0x10, 100, 0x48, 10, 0x50, 20, 0x18, 30]],
                )
                .expect("conversation usage");
        }
        let result = tally_profile(
            &profile(&temp.0, Vendor::Antigravity),
            MachineName::new("max").expect("machine"),
            &TallyOptions::full_history(),
        )
        .expect("tally");
        assert_eq!(result.grains.len(), 1);
        assert_eq!(result.grains[0].model, "antigravity-unknown");
        assert_eq!(result.grains[0].tokens_in, 100);
        assert_eq!(result.grains[0].tokens_out, 30);
        assert_eq!(result.coarse[0].tokens_in, 100);
        assert_eq!(result.coarse[0].tokens_out, 30);
        assert_eq!(
            result.grains[0].settings_hash,
            result.grains[0].settings.sha256().expect("settings hash")
        );
    }

    #[test]
    fn accumulator_rejects_overflow_and_output_row_exhaustion() {
        let profile = profile(Path::new("/tmp"), Vendor::AnthropicOauth);
        let mut tally = FineTally::new(
            &profile,
            MachineName::new("max").expect("machine"),
            1,
            None,
            false,
        );
        let dimensions = || EventDimensions {
            day: "2026-08-08".to_owned(),
            session: "s".to_owned(),
            model: "m".to_owned(),
            settings: AgentSettings::default(),
        };
        tally
            .add(
                dimensions(),
                TokenDelta {
                    input: u64::MAX,
                    ..TokenDelta::default()
                },
                None,
            )
            .expect("first");
        assert!(
            tally
                .add(
                    dimensions(),
                    TokenDelta {
                        input: 1,
                        ..TokenDelta::default()
                    },
                    None
                )
                .is_err()
        );
        let second = EventDimensions {
            model: "other".to_owned(),
            ..dimensions()
        };
        assert!(tally.add(second, TokenDelta::default(), None).is_err());
    }

    #[test]
    fn oversized_jsonl_lines_fail_before_unbounded_allocation() {
        let temp = TempDirectory::new();
        let oversized = "x".repeat(MAX_LINE_BYTES + 1);
        temp.write("projects/project/session.jsonl", &oversized);
        let error = tally_profile(
            &profile(&temp.0, Vendor::AnthropicOauth),
            MachineName::new("max").expect("machine"),
            &TallyOptions::full_history(),
        )
        .expect_err("line bound");
        assert_eq!(error.kind(), PulseErrorKind::Storage);
    }

    #[test]
    fn vendors_without_local_token_transcripts_are_gracefully_empty() {
        let mut gemini = profile(Path::new("/tmp"), Vendor::Gemini);
        gemini.config_dir = None;
        let tally = tally_profile(
            &gemini,
            MachineName::new("max").expect("machine"),
            &TallyOptions::full_history(),
        )
        .expect("disabled tally");
        assert!(tally.grains.is_empty());
        assert_eq!(tally.stats, TallyStats::default());
    }
}
