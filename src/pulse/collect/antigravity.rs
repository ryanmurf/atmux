//! Read-only Antigravity conversation token extraction.
//!
//! The protobuf layout is undocumented. Parsing therefore fails closed and
//! requires the observed `field3 == field9 + field10` checksum for every usage
//! record. Antigravity does not emit a subscription rate-limit snapshot.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::Path,
    time::{Duration, Instant as MonotonicInstant},
};

use rusqlite::{Connection, OpenFlags, types::ValueRef};

use super::{ScanLimits, open_regular_bounded, scan_regular_files_since};
use crate::pulse::{Instant, PulseError, PulseErrorKind, PulseResult};

pub const EMITS_RATE_LIMIT_SNAPSHOT: bool = false;
pub const UNKNOWN_MODEL: &str = "antigravity-unknown";

const MAX_PROTO_DEPTH: usize = 24;
const MAX_PROTO_FIELDS: usize = 65_536;
const MAX_PROTO_BLOBS: usize = 8_192;
const MAX_PROTO_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_PROTO_BLOB_BYTES: usize = 1024 * 1024;
const MAX_USAGE_TUPLES: usize = 8_192;
const MAX_DB_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DB_ROWS: usize = 8_192;
const MAX_METADATA_ROWS: usize = 2_048;
const MAX_METADATA_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONVERSATIONS: usize = 1_024;
const MIN_UNIX_SECONDS: u64 = 1_600_000_000;
const MAX_UNIX_SECONDS: u64 = 2_500_000_000;
const DB_SCAN: ScanLimits = ScanLimits {
    max_depth: 1,
    max_entries: 4_096,
    max_files: MAX_CONVERSATIONS,
    max_file_bytes: MAX_DB_BYTES,
    max_total_bytes: 512 * 1024 * 1024,
    max_duration: Duration::from_secs(2),
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConversationUsage {
    pub prompt: u64,
    pub output: u64,
    pub thinking: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AntigravityConversation {
    pub session_id: String,
    pub model: String,
    pub usage: ConversationUsage,
    pub day: String,
}

#[derive(Clone, Debug)]
enum FieldValue {
    Varint(u64),
    Text(String),
}

type ProtoFields = BTreeMap<u32, FieldValue>;

#[derive(Default)]
struct ParseBudget {
    fields: usize,
}

/// Extracts and deduplicates strict usage tuples across step payloads.
///
/// # Errors
///
/// Returns an invalid-response error for excessive blobs/bytes/work, malformed
/// protobuf framing, or arithmetic overflow.
pub fn extract_usage_from_payloads(payloads: &[Vec<u8>]) -> PulseResult<Option<ConversationUsage>> {
    validate_blob_set(payloads, MAX_PROTO_TOTAL_BYTES)?;
    let mut budget = ParseBudget::default();
    let mut seen = HashSet::new();
    let mut total = ConversationUsage::default();
    let mut found = false;
    for payload in payloads {
        walk_message(payload, 0, &mut budget, &mut |fields| {
            let Some(prompt) = field_varint(fields, 2) else {
                return Ok(());
            };
            if !(50..2_000_000).contains(&prompt) {
                return Ok(());
            }
            let thinking = field_varint(fields, 9).unwrap_or(0);
            let output = field_varint(fields, 10).unwrap_or(0);
            if thinking == 0 && output == 0 {
                return Ok(());
            }
            let Some(checksum) = field_varint(fields, 3) else {
                return Ok(());
            };
            let Some(expected) = thinking.checked_add(output) else {
                return Err(PulseError::invalid_input(
                    "Antigravity usage checksum overflowed",
                ));
            };
            if checksum != expected {
                return Ok(());
            }
            if seen.len() >= MAX_USAGE_TUPLES {
                return Err(PulseError::invalid_input(
                    "Antigravity usage tuple bound was exceeded",
                ));
            }
            if !seen.insert((prompt, output, thinking)) {
                return Ok(());
            }
            total.prompt = total
                .prompt
                .checked_add(prompt)
                .ok_or_else(|| PulseError::invalid_input("Antigravity prompt total overflowed"))?;
            total.output = total
                .output
                .checked_add(output)
                .ok_or_else(|| PulseError::invalid_input("Antigravity output total overflowed"))?;
            total.thinking = total.thinking.checked_add(thinking).ok_or_else(|| {
                PulseError::invalid_input("Antigravity thinking total overflowed")
            })?;
            found = true;
            Ok(())
        })?;
    }
    Ok(found.then_some(total))
}

/// Resolves the most frequently observed bounded model identifier, preserving
/// first-seen order for ties.
///
/// # Errors
///
/// Returns an invalid-response error for hostile protobuf work/size.
pub fn resolve_conversation_model(metadata: &[Vec<u8>]) -> PulseResult<String> {
    validate_blob_set(metadata, MAX_METADATA_TOTAL_BYTES)?;
    let mut budget = ParseBudget::default();
    let mut counts = BTreeMap::<String, usize>::new();
    let mut order = Vec::new();
    for blob in metadata {
        walk_message(blob, 0, &mut budget, &mut |fields| {
            for value in fields.values() {
                let FieldValue::Text(text) = value else {
                    continue;
                };
                for model in model_ids(text) {
                    if !counts.contains_key(&model) {
                        order.push(model.clone());
                    }
                    let count = counts.entry(model).or_default();
                    *count = count.saturating_add(1);
                }
            }
            Ok(())
        })?;
    }
    let mut best = UNKNOWN_MODEL.to_owned();
    let mut best_count = 0;
    for model in order {
        let count = counts.get(&model).copied().unwrap_or(0);
        if count > best_count {
            best = model;
            best_count = count;
        }
    }
    Ok(best)
}

/// Finds the latest plausible Unix-seconds varint in the payload set.
///
/// # Errors
///
/// Returns an invalid-response error for hostile protobuf work/size.
pub fn extract_latest_timestamp(payloads: &[Vec<u8>]) -> PulseResult<Option<u64>> {
    validate_blob_set(payloads, MAX_PROTO_TOTAL_BYTES)?;
    let mut budget = ParseBudget::default();
    let mut latest = None;
    for payload in payloads {
        walk_message(payload, 0, &mut budget, &mut |fields| {
            for value in fields.values() {
                if let FieldValue::Varint(value) = value
                    && (MIN_UNIX_SECONDS..=MAX_UNIX_SECONDS).contains(value)
                    && latest.is_none_or(|current| *value > current)
                {
                    latest = Some(*value);
                }
            }
            Ok(())
        })?;
    }
    Ok(latest)
}

fn validate_blob_set(blobs: &[Vec<u8>], total_limit: usize) -> PulseResult<()> {
    if blobs.len() > MAX_PROTO_BLOBS {
        return Err(PulseError::invalid_input(
            "Antigravity blob count exceeded its bound",
        ));
    }
    let mut total = 0_usize;
    for blob in blobs {
        if blob.len() > MAX_PROTO_BLOB_BYTES {
            return Err(PulseError::invalid_input(
                "Antigravity blob exceeded its size bound",
            ));
        }
        total = total
            .checked_add(blob.len())
            .ok_or_else(|| PulseError::invalid_input("Antigravity blob size total overflowed"))?;
        if total > total_limit {
            return Err(PulseError::invalid_input(
                "Antigravity blobs exceeded their total size bound",
            ));
        }
    }
    Ok(())
}

fn walk_message(
    input: &[u8],
    depth: usize,
    budget: &mut ParseBudget,
    visit: &mut impl FnMut(&ProtoFields) -> PulseResult<()>,
) -> PulseResult<()> {
    if depth > MAX_PROTO_DEPTH {
        return Err(PulseError::invalid_input(
            "Antigravity protobuf nesting exceeded its bound",
        ));
    }
    let mut position = 0_usize;
    let mut fields = ProtoFields::new();
    while position < input.len() {
        budget.fields = budget.fields.saturating_add(1);
        if budget.fields > MAX_PROTO_FIELDS {
            return Err(PulseError::invalid_input(
                "Antigravity protobuf work exceeded its bound",
            ));
        }
        let tag = read_varint(input, &mut position)?;
        if tag == 0 {
            break;
        }
        let field_number = u32::try_from(tag >> 3)
            .ok()
            .filter(|field| *field > 0 && *field <= 536_870_911)
            .ok_or_else(|| PulseError::invalid_input("Antigravity protobuf field was invalid"))?;
        match u8::try_from(tag & 7).unwrap_or(u8::MAX) {
            0 => {
                fields.insert(
                    field_number,
                    FieldValue::Varint(read_varint(input, &mut position)?),
                );
            }
            1 => skip(input, &mut position, 8)?,
            2 => {
                let length = usize::try_from(read_varint(input, &mut position)?).map_err(|_| {
                    PulseError::invalid_input("Antigravity protobuf length was invalid")
                })?;
                let end = position
                    .checked_add(length)
                    .filter(|end| *end <= input.len())
                    .ok_or_else(|| {
                        PulseError::invalid_input("Antigravity protobuf field was truncated")
                    })?;
                let contents = &input[position..end];
                position = end;
                if is_printable(contents) {
                    if contents.len() <= 256
                        && let Ok(text) = std::str::from_utf8(contents)
                    {
                        fields.insert(field_number, FieldValue::Text(text.to_owned()));
                    }
                } else if !contents.is_empty() {
                    walk_message(contents, depth + 1, budget, visit)?;
                }
            }
            5 => skip(input, &mut position, 4)?,
            _ => {
                return Err(PulseError::invalid_input(
                    "Antigravity protobuf wire type was invalid",
                ));
            }
        }
    }
    visit(&fields)
}

fn read_varint(input: &[u8], position: &mut usize) -> PulseResult<u64> {
    let mut result = 0_u64;
    for index in 0..10_u32 {
        let byte = *input.get(*position).ok_or_else(|| {
            PulseError::invalid_input("Antigravity protobuf varint was truncated")
        })?;
        *position = (*position).saturating_add(1);
        if index == 9 && byte > 1 {
            return Err(PulseError::invalid_input(
                "Antigravity protobuf varint overflowed",
            ));
        }
        result |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(PulseError::invalid_input(
        "Antigravity protobuf varint exceeded its bound",
    ))
}

fn skip(input: &[u8], position: &mut usize, bytes: usize) -> PulseResult<()> {
    *position = position
        .checked_add(bytes)
        .filter(|position| *position <= input.len())
        .ok_or_else(|| PulseError::invalid_input("Antigravity protobuf field was truncated"))?;
    Ok(())
}

fn is_printable(input: &[u8]) -> bool {
    if input.is_empty() {
        return false;
    }
    let printable = input
        .iter()
        .filter(|byte| matches!(byte, b'\t' | b'\n' | b'\r' | 32..=126))
        .count();
    printable.saturating_mul(100) / input.len() >= 85
}

fn field_varint(fields: &ProtoFields, field: u32) -> Option<u64> {
    match fields.get(&field) {
        Some(FieldValue::Varint(value)) => Some(*value),
        _ => None,
    }
}

fn model_ids(text: &str) -> Vec<String> {
    let lowercase = text.to_ascii_lowercase();
    let bytes = lowercase.as_bytes();
    let mut models = Vec::new();
    for (start, _) in lowercase.char_indices() {
        if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
            continue;
        }
        let suffix = &lowercase[start..];
        let family_length = [
            "gemini", "claude", "gpt", "grok", "llama", "deepseek", "gemma", "mistral", "qwen",
        ]
        .into_iter()
        .find(|family| suffix.starts_with(family))
        .map(str::len)
        .or_else(|| {
            (suffix.starts_with('o') && suffix.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
                .then_some(2)
        });
        let Some(family_length) = family_length else {
            continue;
        };
        if !suffix
            .as_bytes()
            .get(family_length)
            .is_some_and(|byte| matches!(byte, b'.' | b'-'))
        {
            continue;
        }
        let end = suffix
            .char_indices()
            .take_while(|(_, character)| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '-')
            })
            .map(|(index, character)| index + character.len_utf8())
            .last()
            .unwrap_or(0)
            .min(40);
        let model = suffix[..end].trim_end_matches(['.', '-']);
        if model.len() >= 4 && !models.iter().any(|existing| existing == model) {
            models.push(model.to_owned());
        }
    }
    models
}

/// Reads one conversation database using `SQLite`'s read-only mode and bounded
/// fixed queries. Symlinks/nonregular/oversized files are refused before open.
///
/// # Errors
///
/// Returns a secret-free invalid/unavailable error for unsafe files, corrupt
/// `SQLite`, excessive rows/bytes/time, or hostile protobuf data.
pub fn tally_conversation_db(
    path: &Path,
    modified_ms: i64,
) -> PulseResult<Option<AntigravityConversation>> {
    let file = open_regular_bounded(path, MAX_DB_BYTES).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PulseError::new(
                PulseErrorKind::NotFound,
                "Antigravity conversation database is absent",
            )
        } else {
            PulseError::invalid_input(
                "Antigravity conversation database was not a bounded regular file",
            )
        }
    })?;
    let started = MonotonicInstant::now();
    let connection = open_read_only_connection(path, &file)?;
    connection.busy_timeout(Duration::ZERO).map_err(|_| {
        PulseError::invalid_input("Antigravity conversation database could not be bounded")
    })?;
    let payloads = query_blobs(
        &connection,
        "SELECT step_payload FROM steps WHERE step_payload IS NOT NULL LIMIT ?1",
        MAX_DB_ROWS,
        MAX_PROTO_TOTAL_BYTES,
        started,
    )?;
    let Some(usage) = extract_usage_from_payloads(&payloads)? else {
        return Ok(None);
    };
    let mut metadata_blobs = Vec::new();
    for (table, query) in [
        (
            "gen_metadata",
            "SELECT data FROM gen_metadata WHERE data IS NOT NULL LIMIT ?1",
        ),
        (
            "executor_metadata",
            "SELECT data FROM executor_metadata WHERE data IS NOT NULL LIMIT ?1",
        ),
    ] {
        if table_exists(&connection, table)? {
            let mut blobs = query_blobs(
                &connection,
                query,
                MAX_METADATA_ROWS,
                MAX_METADATA_TOTAL_BYTES
                    .saturating_sub(metadata_blobs.iter().map(Vec::len).sum::<usize>()),
                started,
            )?;
            metadata_blobs.append(&mut blobs);
        }
    }
    let model = resolve_conversation_model(&metadata_blobs)?;
    let timestamp_ms = extract_latest_timestamp(&payloads)?
        .and_then(|seconds| i64::try_from(seconds).ok())
        .and_then(|seconds| seconds.checked_mul(1_000))
        .unwrap_or(modified_ms);
    let day = Instant::from_epoch_millis(timestamp_ms)?
        .to_iso8601()
        .get(0..10)
        .ok_or_else(|| PulseError::invalid_input("Antigravity activity day was invalid"))?
        .to_owned();
    let session_id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| {
            !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| PulseError::invalid_input("Antigravity session id was invalid"))?
        .to_owned();
    Ok(Some(AntigravityConversation {
        session_id,
        model,
        usage,
        day,
    }))
}

#[cfg(unix)]
fn open_read_only_connection(_path: &Path, file: &fs::File) -> PulseResult<Connection> {
    use std::os::fd::AsRawFd as _;

    let descriptor_path = if cfg!(target_os = "linux") {
        format!("/proc/self/fd/{}", file.as_raw_fd())
    } else {
        format!("/dev/fd/{}", file.as_raw_fd())
    };
    Connection::open_with_flags(
        descriptor_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| PulseError::invalid_input("Antigravity conversation database was invalid"))
}

#[cfg(not(unix))]
fn open_read_only_connection(path: &Path, _file: &fs::File) -> PulseResult<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| PulseError::invalid_input("Antigravity conversation database was invalid"))
}

fn table_exists(connection: &Connection, table: &str) -> PulseResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|_| PulseError::invalid_input("Antigravity database schema was invalid"))
}

fn query_blobs(
    connection: &Connection,
    sql: &str,
    row_limit: usize,
    byte_limit: usize,
    started: MonotonicInstant,
) -> PulseResult<Vec<Vec<u8>>> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|_| PulseError::invalid_input("Antigravity database table was unavailable"))?;
    let query_limit = i64::try_from(row_limit.saturating_add(1)).unwrap_or(i64::MAX);
    let mut rows = statement
        .query([query_limit])
        .map_err(|_| PulseError::invalid_input("Antigravity database query failed"))?;
    let mut blobs = Vec::new();
    let mut bytes = 0_usize;
    while let Some(row) = rows
        .next()
        .map_err(|_| PulseError::invalid_input("Antigravity database row was invalid"))?
    {
        if started.elapsed() > Duration::from_secs(2) {
            return Err(PulseError::new(
                PulseErrorKind::Storage,
                "Antigravity database read exceeded its time bound",
            ));
        }
        if blobs.len() >= row_limit {
            return Err(PulseError::invalid_input(
                "Antigravity database row bound was exceeded",
            ));
        }
        let ValueRef::Blob(blob) = row
            .get_ref(0)
            .map_err(|_| PulseError::invalid_input("Antigravity database value was invalid"))?
        else {
            return Err(PulseError::invalid_input(
                "Antigravity database value was not a blob",
            ));
        };
        if blob.len() > MAX_PROTO_BLOB_BYTES {
            return Err(PulseError::invalid_input(
                "Antigravity database blob exceeded its size bound",
            ));
        }
        bytes = bytes
            .checked_add(blob.len())
            .filter(|bytes| *bytes <= byte_limit)
            .ok_or_else(|| {
                PulseError::invalid_input("Antigravity database blobs exceeded their size bound")
            })?;
        blobs.push(blob.to_vec());
    }
    Ok(blobs)
}

/// Discovers and tallies a bounded set of regular conversation databases.
/// Duplicate session ids are collapsed deterministically to the newest file.
///
/// # Errors
///
/// Returns a bounded scan/database/parser error. Missing conversation storage
/// is a graceful empty result.
pub fn collect_conversations(
    config_dir: &Path,
    since_ms: Option<i64>,
) -> PulseResult<Vec<AntigravityConversation>> {
    let root = config_dir.join("conversations");
    let mut files = match scan_regular_files_since(&root, DB_SCAN, since_ms, |path| {
        path.extension().and_then(|value| value.to_str()) == Some("db")
    }) {
        Ok(files) => files,
        Err(error) if error.kind() == PulseErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    files.sort_by_key(|file| Reverse(file.modified_ms));
    let mut sessions = BTreeSet::new();
    let mut conversations = Vec::new();
    for file in files.into_iter().take(MAX_CONVERSATIONS) {
        let session = file
            .path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if !sessions.insert(session.to_owned()) {
            continue;
        }
        if let Some(conversation) = tally_conversation_db(&file.path, file.modified_ms)? {
            conversations.push(conversation);
        }
    }
    Ok(conversations)
}
