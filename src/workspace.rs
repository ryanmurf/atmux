//! Pane-owned project files and read-only Git inspection.
//!
//! Browser requests carry only an opaque pane id and a relative path. The
//! owning node derives the project root from that pane's live working
//! directory and keeps every filesystem and Git operation inside configured
//! launch roots.

use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, Metadata},
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
    process::ExitStatus,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, RawMode, RenameFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, process::Command};
use xattr::FileExt as _;

const MAX_RELATIVE_PATH_BYTES: usize = 4_096;
const MAX_DIRECTORY_SCAN_ENTRIES: usize = 4_096;
const MAX_DIRECTORY_ENTRIES: usize = 512;
const MAX_DIRECTORY_PATH_BYTES: usize = 320 * 1024;
const MAX_FILE_BYTES: usize = 256 * 1024;
/// A full-size text file and path can each consist entirely of JSON-escaped
/// one-byte characters. Include the fixed field names and hash with margin,
/// while keeping the route independently bounded.
pub const MAX_FILE_WRITE_REQUEST_BYTES: usize =
    (MAX_FILE_BYTES * 2) + (MAX_RELATIVE_PATH_BYTES * 2) + 512;
const MAX_GIT_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_GIT_DIFF_BYTES: usize = 256 * 1024;
const MAX_GIT_CHANGES: usize = 512;
const MAX_GIT_PATH_BYTES: usize = 256 * 1024;
const MAX_GIT_STDERR_BYTES: usize = 8 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const GIT_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
static EDIT_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static FILE_EDIT_GATE: Mutex<()> = Mutex::new(());
const MAX_EDIT_XATTRS: usize = 128;
const MAX_EDIT_XATTR_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceErrorKind {
    Invalid,
    NotFound,
    Conflict,
    Internal,
}

#[derive(Debug)]
pub struct WorkspaceError {
    kind: WorkspaceErrorKind,
    message: String,
}

impl WorkspaceError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: WorkspaceErrorKind::Invalid,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            kind: WorkspaceErrorKind::NotFound,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: WorkspaceErrorKind::Conflict,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: WorkspaceErrorKind::Internal,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> WorkspaceErrorKind {
        self.kind
    }
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkspaceError {}

type WorkspaceResult<T> = Result<T, WorkspaceError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileEntryKind {
    Directory,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub kind: FileEntryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FilesResponse {
    Directory {
        pane_id: String,
        path: String,
        entries: Vec<FileEntry>,
        truncated: bool,
    },
    File {
        pane_id: String,
        path: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        size: u64,
        truncated: bool,
        binary: bool,
        /// SHA-256 of the exact complete file bytes. It is intentionally
        /// absent for binary or truncated reads, which cannot be edited.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<String>,
        /// Logical text-line count for complete editable text files.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line_count: Option<usize>,
    },
}

/// One optimistic, pane-owned update of an existing project text file.
/// Unknown fields are rejected so a caller can never smuggle a root or other
/// filesystem selector through this API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileWriteRequest {
    pub path: String,
    pub content: String,
    pub expected_hash: String,
}

impl FilesResponse {
    #[must_use]
    pub fn with_pane_id(mut self, pane_id: String) -> Self {
        match &mut self {
            Self::Directory {
                pane_id: current, ..
            }
            | Self::File {
                pane_id: current, ..
            } => *current = pane_id,
        }
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitChange {
    /// Git porcelain's two-column index/worktree status (or `??`).
    pub status: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submodule: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct GitSummary {
    pub pane_id: String,
    pub available: bool,
    pub branch: Option<String>,
    pub detached: bool,
    pub clean: bool,
    pub changes: Vec<GitChange>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDiff {
    pub pane_id: String,
    pub path: String,
    pub diff: String,
    pub language: String,
    pub truncated: bool,
    pub binary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GitResponse {
    Summary(GitSummary),
    Diff(GitDiff),
}

impl GitResponse {
    #[must_use]
    pub fn with_pane_id(mut self, pane_id: String) -> Self {
        match &mut self {
            Self::Summary(summary) => summary.pane_id = pane_id,
            Self::Diff(diff) => diff.pane_id = pane_id,
        }
        self
    }
}

/// Reads one bounded directory or UTF-8 file below the live pane's project.
///
/// # Errors
///
/// Returns an error when the relative path is invalid or hidden, the live pane
/// directory is unavailable/outside configured roots, or the bounded read
/// cannot be completed safely.
pub async fn files(
    pane_cwd: PathBuf,
    allowed_roots: Vec<PathBuf>,
    requested: Option<String>,
) -> WorkspaceResult<FilesResponse> {
    tokio::task::spawn_blocking(move || {
        files_blocking(&pane_cwd, &allowed_roots, requested.as_deref())
    })
    .await
    .map_err(|_| WorkspaceError::internal("project file inspection task failed"))?
}

fn files_blocking(
    pane_cwd: &Path,
    allowed_roots: &[PathBuf],
    requested: Option<&str>,
) -> WorkspaceResult<FilesResponse> {
    let relative = validate_relative_path(requested.unwrap_or_default(), true)?;
    ensure_visible_path(&relative)?;
    let root = project_root(pane_cwd, allowed_roots)?;
    let (target, metadata) = open_relative(&root, &relative)?;
    if metadata.is_dir() {
        list_directory(&relative, &target)
    } else if metadata.is_file() {
        read_file(&relative, target, &metadata)
    } else {
        Err(WorkspaceError::not_found(
            "project path is not a regular file or directory",
        ))
    }
}

fn list_directory(relative: &Path, opened: &File) -> WorkspaceResult<FilesResponse> {
    let mut entries = Vec::new();
    let mut truncated = false;
    let mut directory = Dir::read_from(opened)
        .map_err(|_| WorkspaceError::not_found("project directory could not be read"))?;
    let mut scanned = 0_usize;
    while let Some(result) = directory.read() {
        if scanned >= MAX_DIRECTORY_SCAN_ENTRIES {
            truncated = true;
            break;
        }
        scanned += 1;
        let Ok(entry) = result else {
            continue;
        };
        let Ok(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if matches!(name.as_str(), "." | "..") {
            continue;
        }
        if !safe_component(&name) || sensitive_component(&name) {
            continue;
        }
        let child_relative = relative.join(&name);
        if ensure_visible_path(&child_relative).is_err() {
            continue;
        }
        let Ok(directory_fd) = directory.fd() else {
            continue;
        };
        let Ok(stat) =
            rustix::fs::statat(directory_fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
        else {
            continue;
        };
        let file_type = FileType::from_raw_mode(stat.st_mode);
        let kind = if file_type.is_dir() {
            FileEntryKind::Directory
        } else if file_type.is_file() {
            FileEntryKind::File
        } else {
            continue;
        };
        entries.push(FileEntry {
            name,
            path: path_for_response(&child_relative),
            size: (kind == FileEntryKind::File)
                .then(|| u64::try_from(stat.st_size).ok())
                .flatten(),
            // rustix intentionally exposes the platform's native `stat`
            // timestamp layout. Size and type stay portable; a later file
            // read returns authoritative size from its opened descriptor.
            modified_ms: None,
            kind,
        });
    }
    entries.sort_by(|left, right| {
        let left_dir = left.kind == FileEntryKind::Directory;
        let right_dir = right.kind == FileEntryKind::Directory;
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    if entries.len() > MAX_DIRECTORY_ENTRIES {
        entries.truncate(MAX_DIRECTORY_ENTRIES);
        truncated = true;
    }
    let mut path_bytes = 0_usize;
    entries.retain(|entry| {
        let next = entry.name.len().saturating_add(entry.path.len());
        if path_bytes.saturating_add(next) > MAX_DIRECTORY_PATH_BYTES {
            truncated = true;
            false
        } else {
            path_bytes += next;
            true
        }
    });
    Ok(FilesResponse::Directory {
        pane_id: String::new(),
        path: path_for_response(relative),
        entries,
        truncated,
    })
}

fn read_file(relative: &Path, mut file: File, before: &Metadata) -> WorkspaceResult<FilesResponse> {
    let mut bytes = Vec::with_capacity(MAX_FILE_BYTES.saturating_add(1));
    Read::by_ref(&mut file)
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| WorkspaceError::not_found("project file could not be read"))?;
    let truncated = before.len() > MAX_FILE_BYTES as u64 || bytes.len() > MAX_FILE_BYTES;
    bytes.truncate(MAX_FILE_BYTES);
    let (content, binary) = bounded_text(&bytes, truncated);
    let editable = !truncated && !binary;
    Ok(FilesResponse::File {
        pane_id: String::new(),
        path: path_for_response(relative),
        content,
        language: language_hint(relative).map(str::to_owned),
        size: before.len(),
        truncated,
        binary,
        content_hash: editable.then(|| content_hash(&bytes)),
        line_count: editable.then(|| line_count(&bytes)),
    })
}

/// Atomically updates one existing, complete UTF-8 text file below the live
/// pane's owner-derived project root.
///
/// # Errors
///
/// Returns an error when the path or expected hash is invalid, the target is
/// hidden/unsafe/non-text/oversized, the target changed since its hash was
/// issued, or the descriptor-relative atomic replacement cannot be completed.
///
/// The CAS linearizes at the platform's atomic name exchange. Finite external
/// atomic-replace races are detected from the displaced inode and restored.
/// No portable filesystem primitive makes the following verification part of
/// that exchange: a host crash in that narrow interval can leave the new file
/// at the target and the displaced file under the hidden temporary name.
/// Likewise, a non-cooperating writer that keeps the old inode open can modify
/// that displaced inode after the exchange. Atmux requests are process-locked;
/// external writers need their own crash/locking discipline.
pub async fn write_file(
    pane_cwd: PathBuf,
    allowed_roots: Vec<PathBuf>,
    request: FileWriteRequest,
) -> WorkspaceResult<FilesResponse> {
    tokio::task::spawn_blocking(move || {
        write_file_blocking_with_hooks(&pane_cwd, &allowed_roots, &request, || {}, || {})
    })
    .await
    .map_err(|_| WorkspaceError::internal("project file update task failed"))?
}

#[cfg(test)]
fn write_file_blocking_with_before_commit(
    pane_cwd: &Path,
    allowed_roots: &[PathBuf],
    request: &FileWriteRequest,
    before_commit: impl FnOnce(),
) -> WorkspaceResult<FilesResponse> {
    write_file_blocking_with_hooks(pane_cwd, allowed_roots, request, before_commit, || {})
}

fn write_file_blocking_with_hooks(
    pane_cwd: &Path,
    allowed_roots: &[PathBuf],
    request: &FileWriteRequest,
    before_revalidation: impl FnOnce(),
    after_revalidation: impl FnOnce(),
) -> WorkspaceResult<FilesResponse> {
    // Cooperative writes served by this owner process are serialized. This
    // prevents one atmux request from crashing while another request's
    // unverified exchange is temporarily installed. External editors remain
    // protected by the displaced-inode verification and restore protocol.
    let _gate = FILE_EDIT_GATE
        .lock()
        .map_err(|_| WorkspaceError::internal("project file edit gate is unavailable"))?;
    let relative = validate_relative_path(&request.path, false)?;
    ensure_visible_path(&relative)?;
    validate_edit_content(request.content.as_bytes())?;
    validate_content_hash(&request.expected_hash)?;
    let root = project_root(pane_cwd, allowed_roots)?;
    let (parent, target_name, mut target, original) = open_edit_target(&root, &relative)?;
    validate_edit_target(&original)?;
    let original_bytes = read_complete_text(&mut target, &original)?;
    if content_hash(&original_bytes) != request.expected_hash {
        return Err(WorkspaceError::conflict(
            "project file changed since it was opened",
        ));
    }
    let security = read_security_metadata(&target)?;

    let (temp_name, mut temporary) = create_edit_temp(&parent)?;
    let mut cleanup = EditTemp::new(&parent, temp_name.clone());
    prepare_temporary(
        &mut temporary,
        &original,
        &security,
        request.content.as_bytes(),
    )?;

    // This hook is a no-op in production. Tests use it to deterministically
    // prove that a target mutation after the temporary is durable aborts the
    // commit and cleans the temporary file.
    before_revalidation();

    revalidate_edit_path(
        &root,
        &relative,
        &parent,
        &target_name,
        &original,
        &security,
        &request.expected_hash,
    )?;

    // The atomic exchange below is the CAS linearization point. The second
    // hook is test-only and proves that a replacement in the former
    // check-then-rename gap is detected from the inode displaced by EXCHANGE.
    after_revalidation();
    let temporary_metadata = temporary
        .metadata()
        .map_err(|_| WorkspaceError::internal("updated project file could not be inspected"))?;
    exchange_files(&parent, &temp_name, &target_name)
        .map_err(|_| WorkspaceError::internal("project file could not be exchanged atomically"))?;
    let displaced_matches = named_file_matches(
        &parent,
        &temp_name,
        &original,
        &security,
        &request.expected_hash,
    );
    if !displaced_matches {
        if restore_after_failed_exchange(&parent, &temp_name, &target_name, &temporary_metadata)
            .is_err()
        {
            // The temporary name contains a state displaced during recovery.
            // Retain it rather than deleting bytes that may belong to a
            // non-cooperating concurrent writer.
            cleanup.disarm();
            return Err(WorkspaceError::internal(
                "project file conflict recovery could not complete safely",
            ));
        }
        cleanup.remove_and_sync()?;
        return Err(WorkspaceError::conflict(
            "project file changed while the edit was being saved",
        ));
    }
    cleanup.remove_and_sync()?;

    let bytes = request.content.as_bytes();
    Ok(FilesResponse::File {
        pane_id: String::new(),
        path: path_for_response(&relative),
        content: request.content.clone(),
        language: language_hint(&relative).map(str::to_owned),
        size: bytes.len() as u64,
        truncated: false,
        binary: false,
        content_hash: Some(content_hash(bytes)),
        line_count: Some(line_count(bytes)),
    })
}

/// Atomically swaps two existing names. Linux uses `RENAME_EXCHANGE`; Apple
/// platforms use the equivalent `RENAME_SWAP` through rustix. Atmux nodes are
/// currently supported on those two platform families.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn exchange_files(parent: &File, left: &OsStr, right: &OsStr) -> rustix::io::Result<()> {
    rustix::fs::renameat_with(parent, left, parent, right, RenameFlags::EXCHANGE)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn exchange_files(_parent: &File, _left: &OsStr, _right: &OsStr) -> rustix::io::Result<()> {
    Err(rustix::io::Errno::NOSYS)
}

fn named_file_matches(
    parent: &File,
    name: &OsStr,
    expected: &Metadata,
    expected_security: &SecurityMetadata,
    expected_hash: &str,
) -> bool {
    let Ok((mut file, before)) = open_edit_target_at(parent, name) else {
        return false;
    };
    if !same_edit_identity(expected, &before) {
        return false;
    }
    let Ok(bytes) = read_complete_text(&mut file, &before) else {
        return false;
    };
    let Ok(current_security) = read_security_metadata(&file) else {
        return false;
    };
    // Security metadata reads are part of the observed file state. Take the
    // final stat afterwards so a concurrent xattr/ACL mutation changes ctime
    // and invalidates the displaced original before it can be accepted.
    let Ok(after) = file.metadata() else {
        return false;
    };
    same_edit_state(&before, &after)
        && current_security == *expected_security
        && content_hash(&bytes) == expected_hash
}

/// Restores the state displaced by a failed exchange without silently
/// overwriting a later atomic-replace writer. If a writer replaces the target
/// during recovery, its newly displaced inode becomes the next restore
/// candidate. Every finite sequence of atomic replacements therefore ends
/// with the newest candidate back at the target name.
fn restore_after_failed_exchange(
    parent: &File,
    temporary_name: &OsStr,
    target_name: &OsStr,
    installed: &Metadata,
) -> WorkspaceResult<()> {
    const MAX_RECOVERY_EXCHANGES: usize = 64;
    let mut expected_target = FileIdentity::from_metadata(installed)?;
    for _ in 0..MAX_RECOVERY_EXCHANGES {
        let candidate = named_identity(parent, temporary_name)?;
        exchange_files(parent, temporary_name, target_name).map_err(|_| {
            WorkspaceError::internal("project file conflict could not be restored atomically")
        })?;
        let displaced = named_identity(parent, temporary_name)?;
        if displaced == expected_target {
            return Ok(());
        }
        // Another atomic replacement won between inspection and exchange.
        // It is now preserved under the temporary name and must be restored
        // next; the candidate we just installed is the expected target.
        expected_target = candidate;
    }
    Err(WorkspaceError::internal(
        "project file remained continuously busy during conflict recovery",
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    file_type: FileType,
}

impl FileIdentity {
    fn from_metadata(metadata: &Metadata) -> WorkspaceResult<Self> {
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            file_type: FileType::from_raw_mode(checked_raw_mode(metadata.mode())?),
        })
    }
}

fn checked_mode<T>(mode: u32) -> Option<T>
where
    T: TryFrom<u32>,
{
    T::try_from(mode).ok()
}

fn checked_raw_mode(mode: u32) -> WorkspaceResult<RawMode> {
    checked_mode(mode).ok_or_else(|| {
        WorkspaceError::internal("project file mode could not be represented safely")
    })
}

fn named_identity(parent: &File, name: &OsStr) -> WorkspaceResult<FileIdentity> {
    let stat = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| WorkspaceError::internal("project file conflict state was unavailable"))?;
    Ok(FileIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
        file_type: FileType::from_raw_mode(stat.st_mode),
    })
}

fn validate_edit_content(bytes: &[u8]) -> WorkspaceResult<()> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(WorkspaceError::invalid("file content exceeds 256 KiB"));
    }
    if bytes
        .iter()
        .any(|byte| *byte == 0 || (*byte < b' ' && !matches!(*byte, b'\n' | b'\r' | b'\t')))
    {
        return Err(WorkspaceError::invalid(
            "file content must be UTF-8 text without NUL bytes",
        ));
    }
    Ok(())
}

fn validate_content_hash(value: &str) -> WorkspaceResult<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(WorkspaceError::invalid(
            "expected_hash must be a lowercase SHA-256 hash",
        ));
    }
    Ok(())
}

fn content_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        0
    } else {
        bytes.split(|byte| *byte == b'\n').count() - usize::from(bytes.ends_with(b"\n"))
    }
}

fn validate_edit_target(metadata: &Metadata) -> WorkspaceResult<()> {
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.len() > MAX_FILE_BYTES as u64
    {
        return Err(WorkspaceError::not_found(
            "project file cannot be edited safely",
        ));
    }
    Ok(())
}

fn read_complete_text(file: &mut File, metadata: &Metadata) -> WorkspaceResult<Vec<u8>> {
    validate_edit_target(metadata)?;
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| WorkspaceError::not_found("project file cannot be edited safely"))?;
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(file)
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| WorkspaceError::not_found("project file could not be read"))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(WorkspaceError::not_found(
            "project file cannot be edited safely",
        ));
    }
    let (text, binary) = bounded_text(&bytes, false);
    if binary || text.len() != bytes.len() {
        return Err(WorkspaceError::invalid(
            "only complete UTF-8 text files can be edited",
        ));
    }
    Ok(bytes)
}

fn prepare_temporary(
    temporary: &mut File,
    original: &Metadata,
    security: &SecurityMetadata,
    bytes: &[u8],
) -> WorkspaceResult<()> {
    temporary
        .write_all(bytes)
        .map_err(|_| WorkspaceError::internal("updated project file could not be written"))?;
    let temporary_metadata = temporary
        .metadata()
        .map_err(|_| WorkspaceError::internal("updated project file could not be inspected"))?;
    if temporary_metadata.gid() != original.gid() {
        rustix::fs::fchown(
            &*temporary,
            None,
            Some(rustix::process::Gid::from_raw(original.gid())),
        )
        .map_err(|_| WorkspaceError::internal("project file ownership could not be preserved"))?;
    }
    let permissions = checked_raw_mode(original.mode() & 0o7_777)?;
    rustix::fs::fchmod(&*temporary, Mode::from_raw_mode(permissions))
        .map_err(|_| WorkspaceError::internal("project file permissions could not be preserved"))?;
    apply_security_metadata(temporary, security)?;
    let copied_security = read_security_metadata(temporary)?;
    let after = temporary
        .metadata()
        .map_err(|_| WorkspaceError::internal("updated project file could not be inspected"))?;
    if after.uid() != original.uid()
        || after.gid() != original.gid()
        || after.mode() != original.mode()
        || copied_security != *security
    {
        return Err(WorkspaceError::internal(
            "project file security metadata could not be preserved",
        ));
    }
    temporary
        .sync_all()
        .map_err(|_| WorkspaceError::internal("updated project file could not be synchronized"))
}

fn open_edit_target(
    root: &Path,
    relative: &Path,
) -> WorkspaceResult<(File, OsString, File, Metadata)> {
    let target_name = relative
        .file_name()
        .ok_or_else(|| WorkspaceError::invalid("path must select a file"))?
        .to_os_string();
    let mut parent = open_absolute_directory(root)?;
    if let Some(parent_path) = relative.parent() {
        for component in parent_path.components() {
            let Component::Normal(name) = component else {
                return Err(WorkspaceError::invalid("path must be relative"));
            };
            parent = open_child_directory(&parent, name)?;
        }
    }
    let (target, metadata) = open_edit_target_at(&parent, &target_name)?;
    Ok((parent, target_name, target, metadata))
}

fn open_child_directory(parent: &File, name: &OsStr) -> WorkspaceResult<File> {
    let before = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| WorkspaceError::not_found("project path was not found"))?;
    if !FileType::from_raw_mode(before.st_mode).is_dir() {
        return Err(WorkspaceError::not_found("project path was not found"));
    }
    let opened = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| WorkspaceError::not_found("project path could not be opened safely"))?;
    let metadata = opened
        .metadata()
        .map_err(|_| WorkspaceError::not_found("project path could not be inspected"))?;
    if before.st_dev as u64 != metadata.dev()
        || before.st_ino as u64 != metadata.ino()
        || !metadata.is_dir()
    {
        return Err(WorkspaceError::not_found("project path was not found"));
    }
    Ok(opened)
}

fn open_edit_target_at(parent: &File, target_name: &OsStr) -> WorkspaceResult<(File, Metadata)> {
    let before = rustix::fs::statat(parent, target_name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| WorkspaceError::not_found("project file was not found"))?;
    if !FileType::from_raw_mode(before.st_mode).is_file() {
        return Err(WorkspaceError::not_found("project file was not found"));
    }
    let target = rustix::fs::openat(
        parent,
        target_name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NOCTTY,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| WorkspaceError::not_found("project file could not be opened safely"))?;
    let metadata = target
        .metadata()
        .map_err(|_| WorkspaceError::not_found("project file could not be inspected"))?;
    if before.st_dev as u64 != metadata.dev()
        || before.st_ino as u64 != metadata.ino()
        || !metadata.is_file()
    {
        return Err(WorkspaceError::not_found("project file was not found"));
    }
    Ok((target, metadata))
}

fn create_edit_temp(parent: &File) -> WorkspaceResult<(OsString, File)> {
    for _ in 0..64 {
        let sequence = EDIT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!(
            ".atmux-edit-{}-{sequence:016x}",
            std::process::id()
        ));
        match rustix::fs::openat(
            parent,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(file) => return Ok((name, File::from(file))),
            Err(error) if error == rustix::io::Errno::EXIST => {}
            Err(_) => {
                return Err(WorkspaceError::internal(
                    "temporary project file could not be created",
                ));
            }
        }
    }
    Err(WorkspaceError::internal(
        "temporary project file name could not be allocated",
    ))
}

fn same_edit_identity(original: &Metadata, current: &Metadata) -> bool {
    original.dev() == current.dev()
        && original.ino() == current.ino()
        && original.uid() == current.uid()
        && original.gid() == current.gid()
        && original.mode() == current.mode()
        && original.nlink() == current.nlink()
        && current.nlink() == 1
        && current.is_file()
}

fn same_edit_state(before: &Metadata, after: &Metadata) -> bool {
    same_edit_identity(before, after)
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

fn revalidate_edit_path(
    root: &Path,
    relative: &Path,
    expected_parent: &File,
    expected_name: &OsStr,
    original: &Metadata,
    expected_security: &SecurityMetadata,
    expected_hash: &str,
) -> WorkspaceResult<()> {
    let changed =
        || WorkspaceError::conflict("project file changed while the edit was being saved");
    let (current_parent, current_name, mut current, current_metadata) =
        open_edit_target(root, relative).map_err(|_| changed())?;
    let expected_parent_metadata = expected_parent.metadata().map_err(|_| changed())?;
    let current_parent_metadata = current_parent.metadata().map_err(|_| changed())?;
    if current_name != expected_name
        || !same_directory_identity(&expected_parent_metadata, &current_parent_metadata)
        || !same_edit_identity(original, &current_metadata)
    {
        return Err(changed());
    }
    let current_bytes =
        read_complete_text(&mut current, &current_metadata).map_err(|_| changed())?;
    if content_hash(&current_bytes) != expected_hash {
        return Err(changed());
    }
    if !read_security_metadata(&current).is_ok_and(|security| security == *expected_security) {
        return Err(changed());
    }
    let after_read = current.metadata().map_err(|_| changed())?;
    if !same_edit_state(&current_metadata, &after_read) {
        return Err(changed());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SecurityMetadata {
    xattrs: Vec<(OsString, Vec<u8>)>,
}

fn read_security_metadata(file: &File) -> WorkspaceResult<SecurityMetadata> {
    #[cfg(target_os = "macos")]
    reject_custom_macos_acl(file)?;
    let listed = file
        .list_xattr()
        .map_err(|_| WorkspaceError::not_found("project file security metadata is unavailable"))?;
    let mut names = Vec::new();
    let mut name_bytes = 0usize;
    for name in listed {
        if names.len() == MAX_EDIT_XATTRS {
            return Err(WorkspaceError::not_found(
                "project file has too much security metadata to edit safely",
            ));
        }
        name_bytes = name_bytes
            .checked_add(name.as_bytes().len())
            .filter(|total| *total <= MAX_EDIT_XATTR_BYTES)
            .ok_or_else(|| {
                WorkspaceError::not_found(
                    "project file has too much security metadata to edit safely",
                )
            })?;
        names.push(name);
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let mut total = name_bytes;
    let mut xattrs = Vec::with_capacity(names.len());
    for name in names {
        let value = file
            .get_xattr(&name)
            .map_err(|_| {
                WorkspaceError::not_found("project file security metadata is unavailable")
            })?
            .ok_or_else(|| WorkspaceError::conflict("project file security metadata changed"))?;
        total = total
            .checked_add(value.len())
            .filter(|total| *total <= MAX_EDIT_XATTR_BYTES)
            .ok_or_else(|| {
                WorkspaceError::not_found(
                    "project file has too much security metadata to edit safely",
                )
            })?;
        xattrs.push((name, value));
    }
    Ok(SecurityMetadata { xattrs })
}

fn apply_security_metadata(file: &File, expected: &SecurityMetadata) -> WorkspaceResult<()> {
    let listed = file
        .list_xattr()
        .map_err(|_| WorkspaceError::internal("temporary file security metadata is unavailable"))?;
    let mut inherited = Vec::new();
    let mut inherited_name_bytes = 0usize;
    for name in listed {
        if inherited.len() == MAX_EDIT_XATTRS {
            return Err(WorkspaceError::internal(
                "temporary file has too much security metadata",
            ));
        }
        inherited_name_bytes = inherited_name_bytes
            .checked_add(name.as_bytes().len())
            .filter(|total| *total <= MAX_EDIT_XATTR_BYTES)
            .ok_or_else(|| {
                WorkspaceError::internal("temporary file has too much security metadata")
            })?;
        inherited.push(name);
    }
    for name in inherited {
        if !expected
            .xattrs
            .iter()
            .any(|(expected_name, _)| *expected_name == name)
        {
            file.remove_xattr(&name).map_err(|_| {
                WorkspaceError::internal("temporary file security metadata could not be cleared")
            })?;
        }
    }
    for (name, value) in &expected.xattrs {
        let current = file.get_xattr(name).map_err(|_| {
            WorkspaceError::internal("temporary file security metadata is unavailable")
        })?;
        if current.as_deref() != Some(value.as_slice()) {
            file.set_xattr(name, value).map_err(|_| {
                WorkspaceError::internal("project file security metadata could not be preserved")
            })?;
        }
    }
    #[cfg(target_os = "macos")]
    clear_custom_macos_acl(file)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn reject_custom_macos_acl(file: &File) -> WorkspaceResult<()> {
    let entries = exacl::getfacl(descriptor_path(file), None)
        .map_err(|_| WorkspaceError::not_found("project file access controls are unavailable"))?;
    if entries.is_empty() {
        Ok(())
    } else {
        Err(WorkspaceError::not_found(
            "project files with custom access controls cannot be edited safely",
        ))
    }
}

#[cfg(target_os = "macos")]
fn clear_custom_macos_acl(file: &File) -> WorkspaceResult<()> {
    let path = descriptor_path(file);
    let entries = exacl::getfacl(&path, None)
        .map_err(|_| WorkspaceError::internal("temporary file access controls are unavailable"))?;
    if !entries.is_empty() {
        exacl::setfacl(&[path], &[], None).map_err(|_| {
            WorkspaceError::internal("temporary file access controls could not be cleared")
        })?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn descriptor_path(file: &File) -> PathBuf {
    use std::os::fd::AsRawFd as _;
    PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
}

fn same_directory_identity(before: &Metadata, after: &Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.file_type() == after.file_type()
        && before.is_dir()
}

struct EditTemp<'a> {
    parent: &'a File,
    name: OsString,
    armed: bool,
}

impl<'a> EditTemp<'a> {
    fn new(parent: &'a File, name: OsString) -> Self {
        Self {
            parent,
            name,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn remove_and_sync(&mut self) -> WorkspaceResult<()> {
        rustix::fs::unlinkat(self.parent, &self.name, AtFlags::empty())
            .map_err(|_| WorkspaceError::internal("temporary project file could not be removed"))?;
        self.armed = false;
        rustix::fs::fsync(self.parent)
            .map_err(|_| WorkspaceError::internal("project directory could not be synchronized"))
    }
}

impl Drop for EditTemp<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = rustix::fs::unlinkat(self.parent, &self.name, AtFlags::empty());
            let _ = rustix::fs::fsync(self.parent);
        }
    }
}

/// Reads a repository summary, or a bounded diff for one freshly reported
/// visible changed path.
///
/// # Errors
///
/// Returns an error when the relative path is invalid, hidden, or not present
/// in a fresh owner-issued status; when the project root is unavailable; or
/// when bounded Git inspection fails.
pub async fn git(
    pane_cwd: PathBuf,
    allowed_roots: Vec<PathBuf>,
    requested: Option<String>,
) -> WorkspaceResult<GitResponse> {
    tokio::time::timeout(
        GIT_REQUEST_TIMEOUT,
        git_inner(pane_cwd, allowed_roots, requested),
    )
    .await
    .map_err(|_| WorkspaceError::internal("Git inspection timed out"))?
}

async fn git_inner(
    pane_cwd: PathBuf,
    allowed_roots: Vec<PathBuf>,
    requested: Option<String>,
) -> WorkspaceResult<GitResponse> {
    let requested = requested
        .map(|requested| {
            let path = validate_relative_path(&requested, false)?;
            ensure_visible_path(&path)?;
            Ok::<_, WorkspaceError>(path_for_response(&path))
        })
        .transpose()?;
    let root = project_root(&pane_cwd, &allowed_roots)?;
    let Some(repo) = repository_root(&root) else {
        if requested.is_some() {
            return Err(WorkspaceError::not_found(
                "Git is not available for this project",
            ));
        }
        return Ok(GitResponse::Summary(unavailable_git()));
    };
    let Some(context) = git_context(&repo, &allowed_roots).await? else {
        if requested.is_some() {
            return Err(WorkspaceError::not_found(
                "Git is not available for this project",
            ));
        }
        return Ok(GitResponse::Summary(unavailable_git()));
    };
    let summary = git_summary(&context).await?;
    let Some(requested) = requested else {
        return Ok(GitResponse::Summary(summary));
    };
    let selected = summary
        .changes
        .iter()
        .find(|change| {
            change.path == requested || change.old_path.as_deref() == Some(requested.as_str())
        })
        .ok_or_else(|| {
            WorkspaceError::not_found("Git path is not a currently changed visible file")
        })?;
    let response_path = selected.path.clone();
    let diff = if selected.status == "??" {
        GitDiff {
            pane_id: String::new(),
            path: response_path,
            diff: "Untracked file; no Git diff is available.".to_owned(),
            language: "diff".to_owned(),
            truncated: false,
            binary: false,
        }
    } else {
        git_diff(&context, selected).await?
    };
    Ok(GitResponse::Diff(diff))
}

fn unavailable_git() -> GitSummary {
    GitSummary {
        pane_id: String::new(),
        available: false,
        branch: None,
        detached: false,
        clean: true,
        changes: Vec::new(),
        truncated: false,
    }
}

#[derive(Debug)]
struct GitContext {
    worktree: PathBuf,
    git_dir: PathBuf,
}

async fn git_summary(context: &GitContext) -> WorkspaceResult<GitSummary> {
    let status = run_git_in(
        &context.worktree,
        Some(&context.git_dir),
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--renames",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ],
        MAX_GIT_OUTPUT_BYTES,
    )
    .await?;
    if !status.status.success() {
        return Ok(unavailable_git());
    }
    let parsed = parse_porcelain(&status.stdout, status.truncated);

    let symbolic = run_git_in(
        &context.worktree,
        Some(&context.git_dir),
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        512,
    )
    .await?;
    let (branch, detached) = if symbolic.status.success() {
        (safe_git_label(&symbolic.stdout), false)
    } else {
        let commit = run_git_in(
            &context.worktree,
            Some(&context.git_dir),
            &["rev-parse", "--short=12", "HEAD"],
            512,
        )
        .await?;
        (safe_git_label(&commit.stdout), commit.status.success())
    };
    Ok(GitSummary {
        pane_id: String::new(),
        available: true,
        branch,
        detached,
        clean: !parsed.saw_change,
        changes: parsed.changes,
        truncated: parsed.truncated,
    })
}

async fn git_context(
    repo: &Path,
    allowed_roots: &[PathBuf],
) -> WorkspaceResult<Option<GitContext>> {
    let output = run_git_in(
        repo,
        None,
        &["rev-parse", "--show-toplevel"],
        MAX_RELATIVE_PATH_BYTES,
    )
    .await?;
    if !output.status.success() {
        return Ok(None);
    }
    let Ok(reported) = std::str::from_utf8(&output.stdout) else {
        return Ok(None);
    };
    let reported = Path::new(reported.trim());
    if !reported.is_absolute() || reported.as_os_str().is_empty() {
        return Ok(None);
    }
    let Ok(reported) = reported.canonicalize() else {
        return Ok(None);
    };
    let Ok(expected) = repo.canonicalize() else {
        return Ok(None);
    };
    if reported != expected {
        return Ok(None);
    }

    let git_dir = run_git_in(
        repo,
        None,
        &["rev-parse", "--absolute-git-dir"],
        MAX_RELATIVE_PATH_BYTES,
    )
    .await?;
    if !git_dir.status.success() {
        return Ok(None);
    }
    let Ok(git_dir) = std::str::from_utf8(&git_dir.stdout) else {
        return Ok(None);
    };
    let Ok(git_dir) = Path::new(git_dir.trim()).canonicalize() else {
        return Ok(None);
    };
    if !git_dir.is_dir()
        || !allowed_roots
            .iter()
            .filter_map(|root| root.canonicalize().ok())
            .any(|root| git_dir == root || git_dir.starts_with(root))
    {
        return Ok(None);
    }
    Ok(Some(GitContext {
        worktree: expected,
        git_dir,
    }))
}

async fn git_diff(context: &GitContext, selected: &GitChange) -> WorkspaceResult<GitDiff> {
    const STAGED_HEADER: &[u8] = b"# Staged changes\n";
    const WORKTREE_HEADER: &[u8] = b"# Working tree changes\n";
    let per_diff_limit = (MAX_GIT_DIFF_BYTES - STAGED_HEADER.len() - WORKTREE_HEADER.len()) / 2;
    let cached = run_git_in(
        &context.worktree,
        Some(&context.git_dir),
        &[
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--unified=3",
            "--",
            selected.path.as_str(),
        ],
        per_diff_limit,
    )
    .await?;
    let worktree = run_git_in(
        &context.worktree,
        Some(&context.git_dir),
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--unified=3",
            "--",
            selected.path.as_str(),
        ],
        per_diff_limit,
    )
    .await?;
    let mut stdout = Vec::with_capacity(
        STAGED_HEADER.len() + cached.stdout.len() + WORKTREE_HEADER.len() + worktree.stdout.len(),
    );
    stdout.extend_from_slice(STAGED_HEADER);
    stdout.extend_from_slice(&cached.stdout);
    stdout.extend_from_slice(WORKTREE_HEADER);
    stdout.extend_from_slice(&worktree.stdout);
    let output = GitOutput {
        status: worktree.status,
        stdout,
        truncated: cached.truncated || worktree.truncated,
    };
    let (diff, binary) = bounded_text(&output.stdout, output.truncated);
    Ok(GitDiff {
        pane_id: String::new(),
        path: selected.path.clone(),
        diff,
        language: "diff".to_owned(),
        truncated: output.truncated,
        binary,
    })
}

#[derive(Debug)]
struct ParsedStatus {
    changes: Vec<GitChange>,
    saw_change: bool,
    truncated: bool,
}

fn parse_porcelain(bytes: &[u8], output_truncated: bool) -> ParsedStatus {
    let records = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut changes = Vec::new();
    let mut saw_change = false;
    let mut hidden = false;
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        saw_change = true;
        let parsed = match record.first().copied() {
            Some(b'1') => parse_status_record(record, 9, None),
            Some(b'2') => {
                let old = records.get(index).copied().unwrap_or_default();
                index = index.saturating_add(1);
                parse_status_record(record, 10, Some(old))
            }
            Some(b'u') => parse_status_record(record, 11, None),
            Some(b'?') if record.starts_with(b"? ") => parse_untracked(&record[2..]),
            _ => None,
        };
        let Some(change) = parsed else {
            hidden = true;
            continue;
        };
        if changes.len() >= MAX_GIT_CHANGES {
            hidden = true;
            continue;
        }
        changes.push(change);
    }
    changes.sort_by(|left, right| {
        left.path
            .to_lowercase()
            .cmp(&right.path.to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut path_bytes = 0_usize;
    changes.retain(|change| {
        let next = change
            .path
            .len()
            .saturating_add(change.old_path.as_ref().map_or(0, String::len));
        if path_bytes.saturating_add(next) > MAX_GIT_PATH_BYTES {
            hidden = true;
            false
        } else {
            path_bytes += next;
            true
        }
    });
    ParsedStatus {
        changes,
        saw_change,
        truncated: output_truncated || hidden,
    }
}

fn parse_status_record(record: &[u8], fields: usize, old: Option<&[u8]>) -> Option<GitChange> {
    let fields = record
        .splitn(fields, |byte| *byte == b' ')
        .collect::<Vec<_>>();
    if fields.len() < 3 {
        return None;
    }
    let status = std::str::from_utf8(fields[1]).ok()?;
    if status.len() != 2 || !status.bytes().all(|byte| byte.is_ascii_graphic()) {
        return None;
    }
    let path = relative_git_path(fields.last()?)?;
    let old_path = match old {
        Some(old) => Some(relative_git_path(old)?),
        None => None,
    };
    if old_path.as_deref().is_some_and(|old| old == path) {
        return None;
    }
    let submodule = std::str::from_utf8(fields[2])
        .ok()
        .filter(|value| value.starts_with('S'))
        .map(str::to_owned);
    Some(GitChange {
        status: status.to_owned(),
        path,
        old_path,
        submodule,
    })
}

fn parse_untracked(path: &[u8]) -> Option<GitChange> {
    Some(GitChange {
        status: "??".to_owned(),
        path: relative_git_path(path)?,
        old_path: None,
        submodule: None,
    })
}

fn relative_git_path(bytes: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(bytes).ok()?;
    let path = validate_relative_path(value, false).ok()?;
    ensure_visible_path(&path).ok()?;
    Some(path_for_response(&path))
}

#[derive(Debug)]
struct GitOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    truncated: bool,
}

async fn run_git_in(
    repo: &Path,
    git_dir: Option<&Path>,
    args: &[&str],
    max_stdout: usize,
) -> WorkspaceResult<GitOutput> {
    let mut command = Command::new("git");
    command
        .arg("--literal-pathspecs")
        .arg("--no-pager")
        // A repository-local core.worktree must never redirect reads outside
        // the owner-derived canonical project. Relative `.` is resolved from
        // the validated per-request cwd.
        .arg("--work-tree=.")
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "core.untrackedCache=false"])
        .args(["-c", "diff.external="]);
    if let Some(git_dir) = git_dir {
        let mut option = OsString::from("--git-dir=");
        option.push(git_dir);
        command.arg(option);
    }
    command
        .args(args)
        .current_dir(repo)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_EXTERNAL_DIFF",
        "GIT_CONFIG_COUNT",
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
    ] {
        command.env_remove(variable);
    }
    let mut child = command
        .spawn()
        .map_err(|_| WorkspaceError::internal("Git inspection could not start"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| WorkspaceError::internal("Git stdout was unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| WorkspaceError::internal("Git stderr was unavailable"))?;
    let stdout_task = tokio::spawn(drain_bounded(stdout, max_stdout));
    let stderr_task = tokio::spawn(drain_bounded(stderr, MAX_GIT_STDERR_BYTES));
    let status = match tokio::time::timeout(GIT_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => return Err(WorkspaceError::internal("Git inspection failed")),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(WorkspaceError::internal("Git inspection timed out"));
        }
    };
    let (stdout, stdout_truncated) = stdout_task
        .await
        .map_err(|_| WorkspaceError::internal("Git output task failed"))?
        .map_err(|_| WorkspaceError::internal("Git output could not be read"))?;
    let (_stderr, _) = stderr_task
        .await
        .map_err(|_| WorkspaceError::internal("Git error output task failed"))?
        .map_err(|_| WorkspaceError::internal("Git error output could not be read"))?;
    Ok(GitOutput {
        status,
        stdout,
        truncated: stdout_truncated,
    })
}

async fn drain_bounded(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(limit.min(16 * 1024));
    let mut truncated = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        let kept = remaining.min(count);
        output.extend_from_slice(&buffer[..kept]);
        truncated |= kept < count;
    }
    Ok((output, truncated))
}

fn project_root(pane_cwd: &Path, allowed_roots: &[PathBuf]) -> WorkspaceResult<PathBuf> {
    let cwd = pane_cwd
        .canonicalize()
        .map_err(|_| WorkspaceError::not_found("pane working directory is unavailable"))?;
    if !cwd.is_dir() {
        return Err(WorkspaceError::not_found(
            "pane working directory is unavailable",
        ));
    }
    let allowed = allowed_roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .filter(|root| root.is_dir() && (cwd == *root || cwd.starts_with(root)))
        .collect::<Vec<_>>();
    if allowed.is_empty() {
        return Err(WorkspaceError::not_found(
            "pane project is outside configured launch roots",
        ));
    }
    let root = repository_root(&cwd).unwrap_or_else(|| cwd.clone());
    if !allowed
        .iter()
        .any(|allowed| root == **allowed || root.starts_with(allowed))
    {
        return Err(WorkspaceError::not_found(
            "pane project is outside configured launch roots",
        ));
    }
    Ok(root)
}

fn repository_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|candidate| {
        let marker = candidate.join(".git");
        let metadata = fs::symlink_metadata(marker).ok()?;
        (!metadata.file_type().is_symlink() && (metadata.is_dir() || metadata.is_file()))
            .then(|| candidate.to_path_buf())
    })
}

fn validate_relative_path(value: &str, allow_empty: bool) -> WorkspaceResult<PathBuf> {
    if value.len() > MAX_RELATIVE_PATH_BYTES
        || value.contains('\0')
        || value.chars().any(char::is_control)
    {
        return Err(WorkspaceError::invalid(
            "path is oversized or contains control characters",
        ));
    }
    if value.is_empty() {
        return allow_empty
            .then(PathBuf::new)
            .ok_or_else(|| WorkspaceError::invalid("path must be relative and non-empty"));
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(WorkspaceError::invalid(
            "path must contain only relative file-name components",
        ));
    }
    Ok(path.to_path_buf())
}

/// Opens the canonical project directory and every requested component
/// descriptor-relatively. No pathname component is followed after its parent
/// descriptor has been obtained, so an intermediate symlink swap cannot move
/// a read outside the project.
fn open_relative(root: &Path, relative: &Path) -> WorkspaceResult<(File, Metadata)> {
    let mut current = open_absolute_directory(root)?;
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(WorkspaceError::invalid("path must be relative"));
        };
        let final_component = index + 1 == components.len();
        let before = rustix::fs::statat(&current, *name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| WorkspaceError::not_found("project path was not found"))?;
        let expected_type = FileType::from_raw_mode(before.st_mode);
        if expected_type.is_symlink()
            || (final_component && !expected_type.is_dir() && !expected_type.is_file())
            || (!final_component && !expected_type.is_dir())
        {
            return Err(WorkspaceError::not_found("project path was not found"));
        }
        let mut flags =
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY;
        if expected_type.is_dir() {
            flags |= OFlags::DIRECTORY;
        }
        current = rustix::fs::openat(&current, *name, flags, Mode::empty())
            .map(File::from)
            .map_err(|_| WorkspaceError::not_found("project path could not be opened safely"))?;
        let metadata = current
            .metadata()
            .map_err(|_| WorkspaceError::not_found("project path could not be inspected"))?;
        if before.st_dev as u64 != metadata.dev()
            || before.st_ino as u64 != metadata.ino()
            || expected_type.is_dir() != metadata.is_dir()
            || expected_type.is_file() != metadata.is_file()
        {
            return Err(WorkspaceError::not_found("project path was not found"));
        }
    }
    let metadata = current
        .metadata()
        .map_err(|_| WorkspaceError::not_found("project path could not be inspected"))?;
    Ok((current, metadata))
}

fn open_absolute_directory(path: &Path) -> WorkspaceResult<File> {
    if !path.is_absolute() {
        return Err(WorkspaceError::not_found(
            "pane project root is unavailable",
        ));
    }
    let expected = fs::symlink_metadata(path)
        .map_err(|_| WorkspaceError::not_found("pane project root is unavailable"))?;
    if expected.file_type().is_symlink() || !expected.is_dir() {
        return Err(WorkspaceError::not_found(
            "pane project root is unavailable",
        ));
    }
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY;
    let mut current = rustix::fs::open("/", flags, Mode::empty())
        .map(File::from)
        .map_err(|_| WorkspaceError::not_found("pane project root is unavailable"))?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = rustix::fs::openat(&current, name, flags, Mode::empty())
                    .map(File::from)
                    .map_err(|_| WorkspaceError::not_found("pane project root is unavailable"))?;
            }
            _ => {
                return Err(WorkspaceError::not_found(
                    "pane project root is unavailable",
                ));
            }
        }
    }
    let actual = current
        .metadata()
        .map_err(|_| WorkspaceError::not_found("pane project root is unavailable"))?;
    ensure_same_file(&expected, &actual)?;
    Ok(current)
}

fn ensure_same_file(before: &Metadata, after: &Metadata) -> WorkspaceResult<()> {
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.file_type() != after.file_type()
    {
        return Err(WorkspaceError::not_found(
            "project path changed while it was being read",
        ));
    }
    Ok(())
}

fn ensure_visible_path(path: &Path) -> WorkspaceResult<()> {
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(WorkspaceError::invalid("path must be relative"));
        };
        let Some(component) = component.to_str() else {
            return Err(WorkspaceError::not_found("project path was not found"));
        };
        if !safe_component(component) || sensitive_component(component) {
            return Err(WorkspaceError::not_found("project path was not found"));
        }
    }
    Ok(())
}

fn safe_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && !component.chars().any(char::is_control)
}

fn sensitive_component(component: &str) -> bool {
    let lower = component.to_ascii_lowercase();
    if lower.starts_with(".atmux-edit-") {
        return true;
    }
    if matches!(
        lower.as_str(),
        ".git"
            | ".ssh"
            | ".aws"
            | ".gnupg"
            | ".kube"
            | ".npmrc"
            | ".pypirc"
            | ".netrc"
            | "id_rsa"
            | "id_ed25519"
    ) {
        return true;
    }
    if lower == ".env" || lower.starts_with(".env.") {
        return ![".example", ".sample", ".template"]
            .iter()
            .any(|suffix| lower.ends_with(suffix));
    }
    Path::new(&lower)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "pem" | "key" | "p12" | "pfx"))
}

fn path_for_response(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn bounded_text(bytes: &[u8], was_truncated: bool) -> (String, bool) {
    if bytes
        .iter()
        .any(|byte| *byte == 0 || (*byte < b' ' && !matches!(*byte, b'\n' | b'\r' | b'\t')))
    {
        return (String::new(), true);
    }
    match std::str::from_utf8(bytes) {
        Ok(content) => (content.to_owned(), false),
        Err(error) if was_truncated && error.error_len().is_none() => (
            String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned(),
            false,
        ),
        Err(_) => (String::new(), true),
    }
}

fn safe_git_label(bytes: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(bytes).ok()?.trim();
    (!value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}

fn language_hint(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    match name.as_str() {
        "dockerfile" => return Some("dockerfile"),
        "makefile" => return Some("makefile"),
        "cargo.toml" | "cargo.lock" => return Some("toml"),
        ".gitignore" | ".gitattributes" => return Some("gitignore"),
        _ => {}
    }
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "ts" | "mts" | "cts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "jsx" => Some("jsx"),
        "py" => Some("python"),
        "rb" => Some("ruby"),
        "go" => Some("go"),
        "java" => Some("java"),
        "kt" | "kts" => Some("kotlin"),
        "c" | "h" => Some("c"),
        "cc" | "cpp" | "cxx" | "hpp" => Some("cpp"),
        "cs" => Some("csharp"),
        "sh" | "bash" | "zsh" => Some("shell"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "xml" => Some("xml"),
        "html" | "htm" => Some("html"),
        "css" => Some("css"),
        "scss" => Some("scss"),
        "sql" => Some("sql"),
        "md" | "markdown" => Some("markdown"),
        "diff" | "patch" => Some("diff"),
        "graphql" | "gql" => Some("graphql"),
        "proto" => Some("protobuf"),
        _ => Some("text"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::symlink,
        process::Command as StdCommand,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn fixture(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "atmux-workspace-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn files_at(root: &Path, path: Option<&str>) -> WorkspaceResult<FilesResponse> {
        files_blocking(root, &[root.to_path_buf()], path)
    }

    #[test]
    fn mode_conversion_is_checked_before_using_platform_raw_mode() {
        assert_eq!(checked_mode::<u16>(0o100_644), Some(0o100_644));
        assert_eq!(checked_mode::<u16>(u32::from(u16::MAX) + 1), None);

        let raw_mode = checked_raw_mode(0o100_644).unwrap();
        assert!(FileType::from_raw_mode(raw_mode).is_file());
    }

    #[test]
    fn file_browser_is_relative_bounded_sorted_and_syntax_aware() {
        let root = fixture("files");
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("Zoo.txt"), "z").unwrap();
        fs::write(root.join("alpha.txt"), "a").unwrap();

        let FilesResponse::Directory { entries, .. } = files_at(&root, None).unwrap() else {
            panic!("expected directory")
        };
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["src", "alpha.txt", "Zoo.txt"]
        );
        let FilesResponse::File {
            content,
            language,
            binary,
            ..
        } = files_at(&root, Some("src/main.rs")).unwrap()
        else {
            panic!("expected file")
        };
        assert_eq!(content, "fn main() {}\n");
        assert_eq!(language.as_deref(), Some("rust"));
        assert!(!binary);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_edit_is_hash_guarded_atomic_exact_and_mode_preserving() {
        use xattr::FileExt as _;

        let root = fixture("file-edit");
        let path = root.join("main.rs");
        fs::write(&path, "fn old() {}\n").unwrap();
        File::open(&path)
            .unwrap()
            .set_xattr("user.atmux-test", b"preserved")
            .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o750);
        fs::set_permissions(&path, permissions).unwrap();
        let FilesResponse::File {
            content_hash,
            line_count,
            ..
        } = files_at(&root, Some("main.rs")).unwrap()
        else {
            panic!("expected file")
        };
        assert_eq!(line_count, Some(1));
        let expected_hash = content_hash.unwrap();
        let request = FileWriteRequest {
            path: "main.rs".to_owned(),
            content: "fn new() {\n\tprintln!(\"exact\");\n}\n".to_owned(),
            expected_hash: expected_hash.clone(),
        };
        let FilesResponse::File {
            content,
            content_hash,
            line_count,
            ..
        } = write_file_blocking_with_before_commit(
            &root,
            std::slice::from_ref(&root),
            &request,
            || {},
        )
        .unwrap()
        else {
            panic!("expected updated file")
        };
        assert_eq!(content, request.content);
        assert_eq!(fs::read(&path).unwrap(), request.content.as_bytes());
        assert_eq!(
            content_hash,
            Some(super::content_hash(request.content.as_bytes()))
        );
        assert_eq!(line_count, Some(3));
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&path).unwrap().permissions())
                & 0o777,
            0o750
        );
        assert_eq!(
            File::open(&path)
                .unwrap()
                .get_xattr("user.atmux-test")
                .unwrap()
                .as_deref(),
            Some(b"preserved".as_slice())
        );

        let stale = write_file_blocking_with_before_commit(
            &root,
            std::slice::from_ref(&root),
            &FileWriteRequest {
                content: "stale\n".to_owned(),
                ..request
            },
            || {},
        )
        .unwrap_err();
        assert_eq!(stale.kind(), WorkspaceErrorKind::Conflict);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edit_conflict_after_temp_sync_keeps_external_change_and_cleans_temp() {
        let root = fixture("file-edit-conflict");
        let path = root.join("notes.txt");
        fs::write(&path, "original\n").unwrap();
        let expected_hash = super::content_hash(b"original\n");
        let result = write_file_blocking_with_before_commit(
            &root,
            std::slice::from_ref(&root),
            &FileWriteRequest {
                path: "notes.txt".to_owned(),
                content: "atmux edit\n".to_owned(),
                expected_hash,
            },
            || fs::write(&path, "external edit\n").unwrap(),
        );
        assert_eq!(result.unwrap_err().kind(), WorkspaceErrorKind::Conflict);
        assert_eq!(fs::read_to_string(&path).unwrap(), "external edit\n");
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".atmux-edit-")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn atomic_replacement_after_final_revalidation_is_restored_as_conflict() {
        let root = fixture("file-edit-post-validation-race");
        let path = root.join("notes.txt");
        let replacement = root.join("external-replacement.txt");
        fs::write(&path, "original\n").unwrap();
        let result = write_file_blocking_with_hooks(
            &root,
            std::slice::from_ref(&root),
            &FileWriteRequest {
                path: "notes.txt".to_owned(),
                content: "atmux edit\n".to_owned(),
                expected_hash: super::content_hash(b"original\n"),
            },
            || {},
            || {
                fs::write(&replacement, "external edit\n").unwrap();
                fs::rename(&replacement, &path).unwrap();
            },
        );
        assert_eq!(result.unwrap_err().kind(), WorkspaceErrorKind::Conflict);
        assert_eq!(fs::read_to_string(&path).unwrap(), "external edit\n");
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".atmux-edit-")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn simultaneous_same_hash_edits_serialize_one_winner_and_one_conflict() {
        use std::sync::{Arc, Barrier};

        let root = fixture("file-edit-same-hash");
        fs::write(root.join("notes.txt"), "original\n").unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let spawn = |content: &'static str| {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                write_file_blocking_with_hooks(
                    &root,
                    std::slice::from_ref(&root),
                    &FileWriteRequest {
                        path: "notes.txt".to_owned(),
                        content: content.to_owned(),
                        expected_hash: super::content_hash(b"original\n"),
                    },
                    || {},
                    || {},
                )
            })
        };
        let first = spawn("first\n");
        let second = spawn("second\n");
        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| result
                    .as_ref()
                    .is_err_and(|error| { error.kind() == WorkspaceErrorKind::Conflict }))
                .count(),
            1
        );
        assert!(matches!(
            fs::read_to_string(root.join("notes.txt")).unwrap().as_str(),
            "first\n" | "second\n"
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edit_conflict_on_intermediate_symlink_swap_never_writes_outside() {
        let base = fixture("file-edit-parent-swap");
        let root = base.join("project");
        let outside = base.join("outside");
        let moved = root.join("moved");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("src/notes.txt"), "inside\n").unwrap();
        fs::write(outside.join("notes.txt"), "outside\n").unwrap();

        let result = write_file_blocking_with_before_commit(
            &root,
            std::slice::from_ref(&base),
            &FileWriteRequest {
                path: "src/notes.txt".to_owned(),
                content: "atmux edit\n".to_owned(),
                expected_hash: super::content_hash(b"inside\n"),
            },
            || {
                fs::rename(root.join("src"), &moved).unwrap();
                symlink(&outside, root.join("src")).unwrap();
            },
        );
        assert_eq!(result.unwrap_err().kind(), WorkspaceErrorKind::Conflict);
        assert_eq!(
            fs::read_to_string(moved.join("notes.txt")).unwrap(),
            "inside\n"
        );
        assert_eq!(
            fs::read_to_string(outside.join("notes.txt")).unwrap(),
            "outside\n"
        );
        assert!(fs::read_dir(&moved).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".atmux-edit-")
        }));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn file_edit_rejects_secrets_symlinks_hardlinks_binary_nul_and_oversize() {
        let base = fixture("file-edit-adversarial");
        let root = base.join("project");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("src/ok.txt"), "okay\n").unwrap();
        fs::write(root.join(".env"), "TOKEN=nope\n").unwrap();
        fs::write(root.join("binary.txt"), b"before\0after").unwrap();
        fs::write(outside.join("outside.txt"), "outside\n").unwrap();
        symlink(&outside, root.join("linked-dir")).unwrap();
        symlink(root.join("src/ok.txt"), root.join("linked-file")).unwrap();
        fs::hard_link(root.join("src/ok.txt"), root.join("second-name.txt")).unwrap();

        let request = |path: &str, content: String, expected_hash: String| FileWriteRequest {
            path: path.to_owned(),
            content,
            expected_hash,
        };
        for path in [".env", "linked-dir/outside.txt", "linked-file"] {
            assert!(
                write_file_blocking_with_before_commit(
                    &root,
                    std::slice::from_ref(&base),
                    &request(path, "changed\n".to_owned(), "a".repeat(64)),
                    || {},
                )
                .is_err(),
                "{path}"
            );
        }
        let hardlink = write_file_blocking_with_before_commit(
            &root,
            std::slice::from_ref(&base),
            &request(
                "src/ok.txt",
                "changed\n".to_owned(),
                super::content_hash(b"okay\n"),
            ),
            || {},
        )
        .unwrap_err();
        assert_eq!(hardlink.kind(), WorkspaceErrorKind::NotFound);
        let binary = write_file_blocking_with_before_commit(
            &root,
            std::slice::from_ref(&base),
            &request(
                "binary.txt",
                "changed\n".to_owned(),
                super::content_hash(b"before\0after"),
            ),
            || {},
        )
        .unwrap_err();
        assert_eq!(binary.kind(), WorkspaceErrorKind::Invalid);
        for content in ["bad\0text".to_owned(), "x".repeat(MAX_FILE_BYTES + 1)] {
            let invalid = write_file_blocking_with_before_commit(
                &root,
                std::slice::from_ref(&base),
                &request("second-name.txt", content, "a".repeat(64)),
                || {},
            )
            .unwrap_err();
            assert_eq!(invalid.kind(), WorkspaceErrorKind::Invalid);
        }
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn traversal_symlinks_special_files_and_secrets_fail_closed() {
        let base = fixture("adversarial");
        let root = base.join("project");
        let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "nope").unwrap();
        symlink(outside.join("secret.txt"), root.join("escape")).unwrap();
        symlink(&outside, root.join("escape-dir")).unwrap();
        symlink(root.join("ordinary.txt"), root.join("inside-link")).unwrap();
        fs::write(root.join("ordinary.txt"), "okay").unwrap();
        fs::write(root.join(".env"), "TOKEN=nope").unwrap();
        fs::write(root.join("private.pem"), "nope").unwrap();
        fs::write(root.join(".eslintrc"), "{}").unwrap();
        let fifo = root.join("pipe");
        let status = StdCommand::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());

        for invalid in [
            "/etc/passwd",
            "../outside/secret.txt",
            "src/../../etc",
            ".",
            "a\0b",
        ] {
            assert!(files_at(&root, Some(invalid)).is_err(), "{invalid}");
        }
        for hidden in [
            "escape",
            "escape-dir/secret.txt",
            "inside-link",
            ".env",
            "private.pem",
            "pipe",
        ] {
            assert!(files_at(&root, Some(hidden)).is_err(), "{hidden}");
        }
        assert!(files_at(&root, Some(".eslintrc")).is_ok());
        let FilesResponse::Directory { entries, .. } = files_at(&root, None).unwrap() else {
            panic!("expected directory")
        };
        assert!(entries.iter().all(|entry| !matches!(
            entry.name.as_str(),
            "escape" | "escape-dir" | "inside-link" | ".env" | "private.pem" | "pipe"
        )));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn large_and_binary_files_are_bounded_without_returning_bytes() {
        let root = fixture("bounds");
        fs::write(root.join("large.txt"), vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
        fs::write(root.join("nul.txt"), b"before\0after").unwrap();
        fs::write(root.join("invalid.txt"), [0xff, 0xfe]).unwrap();
        let FilesResponse::File {
            content,
            truncated,
            binary,
            ..
        } = files_at(&root, Some("large.txt")).unwrap()
        else {
            panic!("expected file")
        };
        assert_eq!(content.len(), MAX_FILE_BYTES);
        assert!(truncated);
        assert!(!binary);
        for name in ["nul.txt", "invalid.txt"] {
            let FilesResponse::File {
                content, binary, ..
            } = files_at(&root, Some(name)).unwrap()
            else {
                panic!("expected file")
            };
            assert!(content.is_empty());
            assert!(binary);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn listing_caps_results_and_marks_truncation() {
        let root = fixture("listing-cap");
        for index in 0..(MAX_DIRECTORY_ENTRIES + 5) {
            fs::write(root.join(format!("file-{index:04}.txt")), "x").unwrap();
        }
        let FilesResponse::Directory {
            entries, truncated, ..
        } = files_at(&root, None).unwrap()
        else {
            panic!("expected directory")
        };
        assert_eq!(entries.len(), MAX_DIRECTORY_ENTRIES);
        assert!(truncated);
        fs::remove_dir_all(root).unwrap();
    }

    fn git(root: &Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .arg("--literal-pathspecs")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Atmux Test")
            .env("GIT_AUTHOR_EMAIL", "atmux@example.test")
            .env("GIT_COMMITTER_NAME", "Atmux Test")
            .env("GIT_COMMITTER_EMAIL", "atmux@example.test")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn git_summary_and_diff_handle_spaces_renames_and_sensitive_paths() {
        let root = fixture("git");
        git(&root, &["init", "-q"]);
        fs::write(root.join("old name.rs"), "fn old() {}\n").unwrap();
        fs::write(root.join("cancel.rs"), "original\n").unwrap();
        fs::write(root.join(":(exclude)*"), "magic old\n").unwrap();
        fs::write(root.join(".env"), "TOKEN=old\n").unwrap();
        git(
            &root,
            &[
                "add",
                "--",
                "old name.rs",
                "cancel.rs",
                ":(exclude)*",
                ".env",
            ],
        );
        git(&root, &["commit", "-qm", "initial"]);
        git(&root, &["mv", "--", "old name.rs", "new name.rs"]);
        fs::write(root.join("new name.rs"), "fn new() {}\n").unwrap();
        fs::write(root.join("cancel.rs"), "staged\n").unwrap();
        git(&root, &["add", "--", "cancel.rs"]);
        fs::write(root.join("cancel.rs"), "original\n").unwrap();
        fs::write(root.join(":(exclude)*"), "magic new\n").unwrap();
        fs::write(root.join(".env"), "TOKEN=new\n").unwrap();
        fs::write(root.join("-leading.ts"), "export {};\n").unwrap();

        let hook = root.join("unexpected-hook.sh");
        let hook_marker = root.join("hook-ran");
        let outside_worktree = root.with_extension("outside-worktree");
        fs::create_dir_all(&outside_worktree).unwrap();
        fs::write(
            outside_worktree.join("outside-secret.txt"),
            "do not expose\n",
        )
        .unwrap();
        fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\n", hook_marker.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
        fs::set_permissions(&hook, permissions).unwrap();
        git(&root, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
        git(&root, &["config", "diff.external", hook.to_str().unwrap()]);
        git(
            &root,
            &["config", "diff.evil.textconv", hook.to_str().unwrap()],
        );
        git(
            &root,
            &[
                "config",
                "core.worktree",
                outside_worktree.to_str().unwrap(),
            ],
        );
        fs::write(root.join(".gitattributes"), "*.rs diff=evil\n").unwrap();

        let GitResponse::Summary(summary) = super::git(root.clone(), vec![root.clone()], None)
            .await
            .unwrap()
        else {
            panic!("expected summary")
        };
        assert!(summary.available);
        assert!(!summary.clean);
        assert!(summary.changes.iter().all(|change| change.path != ".env"));
        assert!(
            summary
                .changes
                .iter()
                .any(|change| change.path == "new name.rs"
                    && change.old_path.as_deref() == Some("old name.rs"))
        );
        assert!(
            summary
                .changes
                .iter()
                .any(|change| change.path == "-leading.ts")
        );
        assert!(
            summary
                .changes
                .iter()
                .any(|change| change.path == ":(exclude)*")
        );
        assert!(
            summary
                .changes
                .iter()
                .all(|change| change.path != "outside-secret.txt")
        );
        assert!(!hook_marker.exists());

        let GitResponse::Diff(diff) = super::git(
            root.clone(),
            vec![root.clone()],
            Some("new name.rs".to_owned()),
        )
        .await
        .unwrap() else {
            panic!("expected diff")
        };
        assert!(diff.diff.contains("# Staged changes"));
        assert!(diff.diff.contains("# Working tree changes"));
        assert!(diff.diff.contains("fn new()"));
        assert!(!hook_marker.exists());

        let GitResponse::Diff(cancelled) = super::git(
            root.clone(),
            vec![root.clone()],
            Some("cancel.rs".to_owned()),
        )
        .await
        .unwrap() else {
            panic!("expected cancelled staged/worktree diff")
        };
        assert!(cancelled.diff.contains("-original"));
        assert!(cancelled.diff.contains("+staged"));
        assert!(cancelled.diff.contains("-staged"));
        assert!(cancelled.diff.contains("+original"));

        let GitResponse::Diff(magic) = super::git(
            root.clone(),
            vec![root.clone()],
            Some(":(exclude)*".to_owned()),
        )
        .await
        .unwrap() else {
            panic!("expected literal-magic-path diff")
        };
        assert!(magic.diff.contains("magic new"));
        assert!(!hook_marker.exists());
        assert!(
            super::git(root.clone(), vec![root.clone()], Some(".env".to_owned()))
                .await
                .is_err()
        );
        assert!(
            super::git(
                root.clone(),
                vec![root.clone()],
                Some("Cargo.toml".to_owned())
            )
            .await
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside_worktree).unwrap();
    }

    #[tokio::test]
    async fn non_repository_is_available_false_and_outside_roots_is_denied() {
        let base = fixture("non-repo");
        let root = base.join("allowed");
        let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let GitResponse::Summary(summary) = super::git(root.clone(), vec![root.clone()], None)
            .await
            .unwrap()
        else {
            panic!("expected summary")
        };
        assert!(!summary.available);
        assert!(super::git(outside, vec![root], None).await.is_err());
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn gitfile_cannot_redirect_object_reads_outside_configured_roots() {
        let base = fixture("external-gitdir");
        let allowed = base.join("allowed");
        let project = allowed.join("project");
        let private = base.join("private");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&private).unwrap();
        git(&private, &["init", "-q"]);
        fs::write(private.join("private-secret.txt"), "private blob\n").unwrap();
        git(&private, &["add", "--", "private-secret.txt"]);
        git(&private, &["commit", "-qm", "private"]);
        fs::write(
            project.join(".git"),
            format!("gitdir: {}\n", private.join(".git").display()),
        )
        .unwrap();

        let GitResponse::Summary(summary) = super::git(project, vec![allowed], None).await.unwrap()
        else {
            panic!("expected summary")
        };
        assert!(!summary.available);
        assert!(summary.changes.is_empty());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn regular_file_identity_check_detects_replacement() {
        let root = fixture("identity");
        let path = root.join("file.txt");
        let replacement = root.join("replacement.txt");
        fs::write(&path, "first").unwrap();
        let before = fs::symlink_metadata(&path).unwrap();
        // Create the replacement while the original still exists, so the two
        // files are guaranteed to have distinct identities. Deleting and then
        // recreating `path` lets Linux immediately reuse the original inode,
        // which made this regression nondeterministic on CI.
        fs::write(&replacement, "second").unwrap();
        fs::rename(&replacement, &path).unwrap();
        let after = fs::symlink_metadata(&path).unwrap();
        assert!(ensure_same_file(&before, &after).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
