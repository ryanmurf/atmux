use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};

#[cfg(not(unix))]
use std::fs::OpenOptions;
#[cfg(all(unix, not(target_vendor = "apple")))]
use std::os::fd::AsRawFd as _;
#[cfg(unix)]
use std::os::fd::OwnedFd;

use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};

use super::{
    AlertEvent, AlertEventInput, AlertReply, AlertReplyInput, CurrentQuotaWindow,
    IdempotentIngestResult, ImportBatch, ImportBatchResult, ImportProvenance, ImportedAlertEvent,
    ImportedAlertSubscription, ImportedRow, IngestBatch, IngestLimits, IngestReplay, IngestResult,
    IngestToken, MAX_ALERT_REPLIES_PER_EVENT, MAX_ALERT_REPLY_BYTES, MAX_IMPORT_BATCH_ROWS,
    MAX_IMPORT_RECONCILIATION_KEYS, MAX_INGEST_REPLAYS_PER_ACCOUNT,
    MAX_REPORTER_DESTINATIONS_PER_ACCOUNT, MAX_RESET_HORIZON_MILLIS, MAX_RESET_JOBS_PER_ACCOUNT,
    PricingRule, ReporterCursorState, ReporterPendingChunk, ReporterPendingDraft,
    ReporterPendingPage, ReporterStreamKind, ReporterTokenPosition, ResetResumeInput,
    ResetResumeJob, ResetResumeLimits, RetentionResult, Store, StoreFuture,
    StoredAlertSubscription, StoredTokenTotals, StoredUsageSnapshot, TokenBackfillPage,
    TokenBackfillState, TokenReconciliationKey, TokenWriteObservation, migrate,
    validate_pricing_key, validate_reporter_destination, validate_reporter_transition,
};
use crate::pulse::{
    error::{PulseError, PulseErrorKind, PulseResult},
    federation::{
        FederatedPulseRow, FederatedRecord, FederationExportPosition, FederationState,
        LocalFederationRecord, MAX_PAGE_ROWS, OpaqueCursor,
    },
    model::{
        Account, AccountId, AlertSubscription, AlertType, ContextSession, Fraction, GeminiQuota,
        Machine, MachineName, Percent, Profile, ProfileName, ProfileOrigin, QuotaWindow,
        RefreshPolicy, SessionId, TokenGrain, TokenSource, UsageContributor, UsageSnapshot, Vendor,
    },
    time::Instant,
    token::TokenSourceGeneration,
};

const BUSY_TIMEOUT_MS: u64 = 15_000;
const RESET_JITTER_TOLERANCE_MS: i64 = 5 * 60 * 1_000;
const MAX_QUERY_ROWS: usize = 10_000;

/// `SQLite` runtime settings used for local Pulse databases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlitePragmas {
    pub journal_mode: String,
    pub foreign_keys: bool,
    pub busy_timeout_ms: u64,
}

/// Native `SQLite` store. All connection access runs on Tokio's blocking pool.
#[derive(Clone)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    #[cfg(unix)]
    path_guard: Option<Arc<SqlitePathGuard>>,
}

impl SqliteStore {
    /// Opens, configures, and migrates a `SQLite` database.
    ///
    /// # Errors
    ///
    /// Returns a classified storage error when the file, pragmas, or schema
    /// cannot be initialized.
    pub async fn open(path: impl AsRef<Path>) -> PulseResult<Self> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || Self::open_blocking(&path))
            .await
            .map_err(join_error)?
    }

    /// Opens an existing current-schema database without creating, migrating,
    /// changing journal mode, or permitting writes. Operational diagnostics
    /// use this after independently validating the path and schema.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the existing file cannot be opened with
    /// `SQLite`'s read-only and query-only protections.
    pub async fn open_read_only(path: impl AsRef<Path>) -> PulseResult<Self> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || {
            let Some(prepared) = prepare_sqlite_path(&path, false)? else {
                return Err(PulseError::configuration(
                    "an in-memory database cannot be opened read-only",
                ));
            };
            run_sqlite_open_test_hook(&path);
            let connection =
                Connection::open_with_flags(&prepared.open_path, sqlite_open_flags(true))
                    .map_err(sql_error)?;
            #[cfg(unix)]
            prepared.guard.verify()?;
            verify_sqlite_opened(&connection, prepared.identity)?;
            connection
                .busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
                .map_err(sql_error)?;
            connection
                .pragma_update(None, "foreign_keys", "ON")
                .map_err(sql_error)?;
            connection
                .pragma_update(None, "query_only", "ON")
                .map_err(sql_error)?;
            Ok(Self {
                connection: Arc::new(Mutex::new(connection)),
                #[cfg(unix)]
                path_guard: Some(prepared.guard),
            })
        })
        .await
        .map_err(join_error)?
    }

    fn open_blocking(path: &Path) -> PulseResult<Self> {
        if path == Path::new(":memory:") {
            let mut connection = Connection::open_in_memory().map_err(sql_error)?;
            configure(&connection)?;
            migrate::apply(&mut connection)?;
            return Ok(Self {
                connection: Arc::new(Mutex::new(connection)),
                #[cfg(unix)]
                path_guard: None,
            });
        }
        let prepared = prepare_sqlite_path(path, true)?
            .ok_or_else(|| PulseError::configuration("Pulse SQLite operational path is invalid"))?;
        run_sqlite_open_test_hook(path);
        let mut connection =
            Connection::open_with_flags(&prepared.open_path, sqlite_open_flags(false))
                .map_err(sql_error)?;
        #[cfg(unix)]
        prepared.guard.verify()?;
        verify_sqlite_opened(&connection, prepared.identity)?;
        configure(&connection)?;
        migrate::apply(&mut connection)?;
        verify_sqlite_opened(&connection, prepared.identity)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            #[cfg(unix)]
            path_guard: Some(prepared.guard),
        })
    }

    /// Reports the effective `SQLite` safety/concurrency pragmas.
    #[must_use]
    pub fn pragmas(&self) -> StoreFuture<SqlitePragmas> {
        self.run(|connection| {
            let journal_mode = connection
                .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .map_err(sql_error)?;
            let foreign_keys = connection
                .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                .map_err(sql_error)?
                != 0;
            let busy_timeout_ms = u64::try_from(
                connection
                    .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))
                    .map_err(sql_error)?,
            )
            .map_err(|_| {
                PulseError::new(PulseErrorKind::Storage, "SQLite busy_timeout is negative")
            })?;
            Ok(SqlitePragmas {
                journal_mode,
                foreign_keys,
                busy_timeout_ms,
            })
        })
    }

    /// Checkpoints the WAL after revalidating the pinned operational path.
    #[must_use]
    pub fn checkpoint(&self) -> StoreFuture<()> {
        self.run(|connection| {
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .map_err(sql_error)
        })
    }

    fn run<T, F>(&self, operation: F) -> StoreFuture<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> PulseResult<T> + Send + 'static,
    {
        let connection = Arc::clone(&self.connection);
        #[cfg(unix)]
        let path_guard = self.path_guard.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                #[cfg(unix)]
                if let Some(path_guard) = path_guard {
                    path_guard.verify()?;
                }
                let mut connection = connection.lock().map_err(|_| {
                    PulseError::new(PulseErrorKind::Storage, "Pulse SQLite lock was poisoned")
                })?;
                operation(&mut connection)
            })
            .await
            .map_err(join_error)?
        })
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SqliteFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SqliteFileIdentity {
    length: u64,
}

struct PreparedSqlitePath {
    open_path: PathBuf,
    identity: SqliteFileIdentity,
    #[cfg(unix)]
    guard: Arc<SqlitePathGuard>,
}

#[cfg(unix)]
struct SqlitePathGuard {
    absolute_path: PathBuf,
    parent: OwnedFd,
    file_name: std::ffi::OsString,
    parent_identity: SqliteFileIdentity,
    file_identity: SqliteFileIdentity,
}

#[cfg(unix)]
impl SqlitePathGuard {
    fn verify(&self) -> PulseResult<()> {
        let parent = open_sqlite_parent(&self.absolute_path, false)?;
        if descriptor_identity(&parent)? != self.parent_identity {
            return Err(sqlite_path_changed());
        }
        let file = open_sqlite_file(&parent, &self.file_name, false, false)?;
        if descriptor_identity(&file)? != self.file_identity {
            return Err(sqlite_path_changed());
        }
        let pinned = open_sqlite_file(&self.parent, &self.file_name, false, false)?;
        if descriptor_identity(&pinned)? != self.file_identity {
            return Err(sqlite_path_changed());
        }
        Ok(())
    }
}

#[cfg(unix)]
fn sqlite_file_identity(metadata: &fs::Metadata) -> SqliteFileIdentity {
    use std::os::unix::fs::MetadataExt as _;
    SqliteFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn sqlite_file_identity(metadata: &fs::Metadata) -> SqliteFileIdentity {
    SqliteFileIdentity {
        length: metadata.len(),
    }
}

#[cfg(unix)]
fn prepare_sqlite_path(requested: &Path, create: bool) -> PulseResult<Option<PreparedSqlitePath>> {
    if requested == Path::new(":memory:") {
        return Ok(None);
    }
    let absolute_path = absolute_sqlite_path(requested)?;
    let parent = open_sqlite_parent(&absolute_path, create)?;
    validate_descriptor_mode(&parent, true)?;
    let file_name = absolute_path
        .file_name()
        .ok_or_else(|| PulseError::configuration("Pulse SQLite path must end in a database file"))?
        .to_os_string();
    let file = open_sqlite_file(&parent, &file_name, true, create)?;
    validate_descriptor_mode(&file, false)?;
    let parent_identity = descriptor_identity(&parent)?;
    let file_identity = descriptor_identity(&file)?;
    #[cfg(target_vendor = "apple")]
    let open_path = absolute_path.clone();
    #[cfg(not(target_vendor = "apple"))]
    let open_path = {
        let descriptor_root = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        let mut path = PathBuf::from(format!("{descriptor_root}/{}", parent.as_raw_fd()));
        path.push(&file_name);
        path
    };
    Ok(Some(PreparedSqlitePath {
        open_path,
        identity: file_identity,
        guard: Arc::new(SqlitePathGuard {
            absolute_path,
            parent,
            file_name,
            parent_identity,
            file_identity,
        }),
    }))
}

#[cfg(unix)]
fn absolute_sqlite_path(requested: &Path) -> PulseResult<PathBuf> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| {
                PulseError::configuration("Pulse SQLite working directory is unavailable")
            })?
            .join(requested)
    };
    if absolute.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(PulseError::configuration(
            "Pulse SQLite path must not contain relative traversal components",
        ));
    }
    Ok(absolute)
}

#[cfg(unix)]
fn open_sqlite_parent(absolute: &Path, create: bool) -> PulseResult<OwnedFd> {
    use rustix::fs::{Mode, OFlags};

    let parent = absolute.parent().ok_or_else(|| {
        PulseError::configuration("Pulse SQLite path must have a parent directory")
    })?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut descriptor = rustix::fs::open("/", flags, Mode::empty()).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Pulse SQLite root directory could not be opened",
        )
    })?;
    for component in parent.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        descriptor = match rustix::fs::openat(&descriptor, name, flags, Mode::empty()) {
            Ok(next) => next,
            Err(rustix::io::Errno::NOENT) if create => {
                match rustix::fs::mkdirat(&descriptor, name, Mode::RWXU) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(_) => {
                        return Err(PulseError::new(
                            PulseErrorKind::Storage,
                            "Pulse SQLite data directory could not be created safely",
                        ));
                    }
                }
                rustix::fs::openat(&descriptor, name, flags, Mode::empty())
                    .map_err(|_| unsafe_sqlite_ancestor())?
            }
            Err(rustix::io::Errno::NOENT) => {
                return Err(PulseError::new(
                    PulseErrorKind::NotFound,
                    "Pulse SQLite database is unavailable",
                ));
            }
            Err(_) => return Err(unsafe_sqlite_ancestor()),
        };
    }
    Ok(descriptor)
}

#[cfg(unix)]
fn open_sqlite_file(
    parent: &impl std::os::fd::AsFd,
    file_name: &std::ffi::OsStr,
    writable: bool,
    create: bool,
) -> PulseResult<OwnedFd> {
    use rustix::fs::{Mode, OFlags};

    let access = if writable {
        OFlags::RDWR
    } else {
        OFlags::RDONLY
    };
    let flags = access | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::openat(parent, file_name, flags, Mode::empty()) {
        Ok(file) => Ok(file),
        Err(rustix::io::Errno::NOENT) if create => rustix::fs::openat(
            parent,
            file_name,
            flags | OFlags::CREATE | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| {
            PulseError::new(
                PulseErrorKind::Storage,
                "Pulse SQLite database could not be created safely",
            )
        }),
        Err(rustix::io::Errno::NOENT) => Err(PulseError::new(
            PulseErrorKind::NotFound,
            "Pulse SQLite database is unavailable",
        )),
        Err(_) => Err(PulseError::configuration(
            "Pulse SQLite database must be a regular non-symlink file",
        )),
    }
}

#[cfg(unix)]
fn validate_descriptor_mode(
    descriptor: &impl std::os::fd::AsFd,
    directory: bool,
) -> PulseResult<()> {
    use rustix::fs::{FileType, Mode};

    let stat = rustix::fs::fstat(descriptor).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Pulse SQLite descriptor could not be inspected",
        )
    })?;
    let file_type = FileType::from_raw_mode(stat.st_mode);
    if (directory && !file_type.is_dir()) || (!directory && !file_type.is_file()) {
        return Err(PulseError::configuration(
            "Pulse SQLite path has an unexpected file type",
        ));
    }
    if Mode::from_raw_mode(stat.st_mode).intersects(Mode::WGRP | Mode::WOTH) {
        return Err(PulseError::configuration(if directory {
            "Pulse SQLite parent directory must not be group- or world-writable"
        } else {
            "Pulse SQLite database must not be group- or world-writable"
        }));
    }
    Ok(())
}

#[cfg(unix)]
fn descriptor_identity(descriptor: &impl std::os::fd::AsFd) -> PulseResult<SqliteFileIdentity> {
    let stat = rustix::fs::fstat(descriptor).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Pulse SQLite descriptor identity could not be inspected",
        )
    })?;
    #[cfg(target_vendor = "apple")]
    let device = u64::try_from(stat.st_dev).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Pulse SQLite descriptor identity is invalid",
        )
    })?;
    #[cfg(not(target_vendor = "apple"))]
    let device = stat.st_dev;
    Ok(SqliteFileIdentity {
        device,
        inode: stat.st_ino,
    })
}

#[cfg(unix)]
fn unsafe_sqlite_ancestor() -> PulseError {
    PulseError::configuration("Pulse SQLite path contains an unsafe ancestor")
}

fn sqlite_path_changed() -> PulseError {
    PulseError::new(
        PulseErrorKind::Conflict,
        "Pulse SQLite path changed after it was securely opened",
    )
}

#[cfg(not(unix))]
fn prepare_sqlite_path(requested: &Path, create: bool) -> PulseResult<Option<PreparedSqlitePath>> {
    if requested == Path::new(":memory:") {
        return Ok(None);
    }
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| {
                PulseError::configuration("Pulse SQLite working directory is unavailable")
            })?
            .join(requested)
    };
    if absolute.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(PulseError::configuration(
            "Pulse SQLite path must not contain relative traversal components",
        ));
    }
    let parent = absolute.parent().ok_or_else(|| {
        PulseError::configuration("Pulse SQLite path must have a parent directory")
    })?;
    let mut current = PathBuf::new();
    for component in parent.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(PulseError::configuration(
                    "Pulse SQLite path contains an unsafe ancestor",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => {
                        return Err(PulseError::new(
                            PulseErrorKind::Storage,
                            "Pulse SQLite data directory could not be created",
                        ));
                    }
                }
                let metadata = fs::symlink_metadata(&current).map_err(|_| {
                    PulseError::new(
                        PulseErrorKind::Storage,
                        "Pulse SQLite data directory could not be verified",
                    )
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(PulseError::configuration(
                        "Pulse SQLite path contains an unsafe ancestor",
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(PulseError::new(
                    PulseErrorKind::NotFound,
                    "Pulse SQLite database is unavailable",
                ));
            }
            Err(_) => {
                return Err(PulseError::new(
                    PulseErrorKind::Storage,
                    "Pulse SQLite path could not be inspected",
                ));
            }
        }
    }
    validate_sqlite_parent(parent)?;
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            options.open(&absolute).map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Storage,
                    "Pulse SQLite database could not be created safely",
                )
            })?;
            fs::symlink_metadata(&absolute).map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Storage,
                    "Pulse SQLite database could not be verified",
                )
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PulseError::new(
                PulseErrorKind::NotFound,
                "Pulse SQLite database is unavailable",
            ));
        }
        Err(_) => {
            return Err(PulseError::new(
                PulseErrorKind::Storage,
                "Pulse SQLite database could not be inspected",
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PulseError::configuration(
            "Pulse SQLite database must be a regular non-symlink file",
        ));
    }
    Ok(Some(PreparedSqlitePath {
        open_path: absolute,
        identity: sqlite_file_identity(&metadata),
    }))
}

#[cfg(not(unix))]
fn validate_sqlite_parent(_parent: &Path) -> PulseResult<()> {
    Ok(())
}

fn verify_sqlite_opened(connection: &Connection, identity: SqliteFileIdentity) -> PulseResult<()> {
    let opened = connection
        .query_row("PRAGMA database_list", [], |row| row.get::<_, String>(2))
        .map_err(sql_error)?;
    let metadata = fs::metadata(opened).map_err(|_| sqlite_path_changed())?;
    if !metadata.is_file() || sqlite_file_identity(&metadata) != identity {
        return Err(sqlite_path_changed());
    }
    Ok(())
}

#[cfg(test)]
type SqliteOpenTestHook = Box<dyn FnOnce() + Send + 'static>;

#[cfg(test)]
static SQLITE_OPEN_TEST_HOOKS: Mutex<Vec<(PathBuf, SqliteOpenTestHook)>> = Mutex::new(Vec::new());

#[cfg(test)]
fn run_sqlite_open_test_hook(path: &Path) {
    let hook = SQLITE_OPEN_TEST_HOOKS.lock().ok().and_then(|mut hooks| {
        let index = hooks.iter().position(|(target, _)| target == path)?;
        Some(hooks.remove(index).1)
    });
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
fn install_sqlite_open_test_hook(path: PathBuf, hook: SqliteOpenTestHook) {
    SQLITE_OPEN_TEST_HOOKS
        .lock()
        .expect("SQLite open test hook lock")
        .push((path, hook));
}

#[cfg(not(test))]
fn run_sqlite_open_test_hook(_path: &Path) {}

fn sqlite_open_flags(read_only: bool) -> rusqlite::OpenFlags {
    let access = if read_only {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let base = access | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    #[cfg(all(unix, not(target_vendor = "apple")))]
    return base;
    #[cfg(target_vendor = "apple")]
    return base | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
    #[cfg(not(unix))]
    (base | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW)
}

fn configure(connection: &Connection) -> PulseResult<()> {
    connection
        .busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(sql_error)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(sql_error)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(sql_error)?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(sql_error)?;
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn join_error(error: tokio::task::JoinError) -> PulseError {
    PulseError::new(
        PulseErrorKind::Internal,
        format!("Pulse SQLite blocking task failed: {error}"),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn sql_error(error: rusqlite::Error) -> PulseError {
    let kind = match &error {
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                ErrorCode::ConstraintViolation
                    | ErrorCode::DatabaseBusy
                    | ErrorCode::DatabaseLocked
            ) =>
        {
            if inner.code == ErrorCode::ConstraintViolation {
                PulseErrorKind::Conflict
            } else {
                PulseErrorKind::Storage
            }
        }
        _ => PulseErrorKind::Storage,
    };
    PulseError::new(kind, format!("Pulse SQLite operation failed: {error}"))
}

fn encode<T: Serialize>(value: &T) -> PulseResult<String> {
    serde_json::to_string(value).map_err(|error| {
        PulseError::new(
            PulseErrorKind::Internal,
            format!("failed to encode typed Pulse value: {error}"),
        )
    })
}

fn decode<T: DeserializeOwned>(value: &str) -> PulseResult<T> {
    serde_json::from_str(value).map_err(|error| {
        PulseError::new(
            PulseErrorKind::Storage,
            format!("stored Pulse value is invalid: {error}"),
        )
    })
}

fn path_text(path: Option<&PathBuf>, field: &str) -> PulseResult<Option<String>> {
    path.map(|value| {
        value
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| PulseError::invalid_input(format!("{field} must be valid UTF-8")))
    })
    .transpose()
}

fn instant(value: i64) -> PulseResult<Instant> {
    Instant::from_epoch_millis(value).map_err(|error| {
        PulseError::new(
            PulseErrorKind::Storage,
            format!("stored Pulse instant is invalid: {error}"),
        )
    })
}

fn as_i64(value: u64, field: &str) -> PulseResult<i64> {
    i64::try_from(value)
        .map_err(|_| PulseError::invalid_input(format!("{field} exceeds SQLite INTEGER")))
}

fn query_limit(value: usize) -> PulseResult<i64> {
    if value == 0 || value > MAX_QUERY_ROWS {
        return Err(PulseError::invalid_input(format!(
            "query limit must be between 1 and {MAX_QUERY_ROWS}"
        )));
    }
    i64::try_from(value).map_err(|_| PulseError::invalid_input("query limit is too large"))
}

struct RawProfile {
    account_id: i64,
    name: String,
    vendor: String,
    config_dir: Option<String>,
    poll_interval_minutes: u32,
    monthly_budget_usd: Option<f64>,
    api_key_env: Option<String>,
    api_key_file: Option<String>,
    refresh: String,
    hidden: bool,
    origin: String,
}

fn raw_profile(row: &Row<'_>) -> rusqlite::Result<RawProfile> {
    Ok(RawProfile {
        account_id: row.get(0)?,
        name: row.get(1)?,
        vendor: row.get(2)?,
        config_dir: row.get(3)?,
        poll_interval_minutes: row.get(4)?,
        monthly_budget_usd: row.get(5)?,
        api_key_env: row.get(6)?,
        api_key_file: row.get(7)?,
        refresh: row.get(8)?,
        hidden: row.get::<_, i64>(9)? != 0,
        origin: row.get(10)?,
    })
}

fn decode_profile(raw: RawProfile) -> PulseResult<Profile> {
    Ok(Profile {
        account_id: AccountId::new(raw.account_id)?,
        name: ProfileName::new(raw.name)?,
        vendor: decode(&raw.vendor)?,
        config_dir: raw.config_dir.map(PathBuf::from),
        poll_interval_minutes: raw.poll_interval_minutes,
        monthly_budget_usd: raw.monthly_budget_usd,
        api_key_env: raw.api_key_env,
        api_key_file: raw.api_key_file.map(PathBuf::from),
        refresh: decode(&raw.refresh)?,
        hidden: raw.hidden,
        origin: decode(&raw.origin)?,
    })
}

const PROFILE_COLUMNS: &str = "account_id, name, vendor_json, config_dir, \
    poll_interval_minutes, monthly_budget_usd, api_key_env, api_key_file, refresh_json, hidden, origin_json";

fn load_federation_state(
    connection: &Connection,
    account_id: AccountId,
    source_machine: &MachineName,
) -> PulseResult<FederationState> {
    let (cursor, generation, pages, records, complete) = connection
        .query_row(
            "SELECT cursor, generation, pages_applied, records_applied, complete \
             FROM federation_peers WHERE account_id=?1 AND source_machine=?2",
            params![account_id.get(), source_machine.as_str()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)? != 0,
                ))
            },
        )
        .map_err(sql_error)?;
    Ok(FederationState {
        cursor: cursor.map(OpaqueCursor::new).transpose()?,
        generation: as_u64(generation)?,
        pages_applied: as_u64(pages)?,
        records_applied: as_u64(records)?,
        complete,
    })
}

fn load_sqlite_reporter_pending(
    connection: &Connection,
    account_id: AccountId,
    machine: &MachineName,
    destination_key: &str,
    kind: ReporterStreamKind,
) -> PulseResult<Option<ReporterPendingPage>> {
    let row = connection
        .query_row(
            "SELECT id,expected_cursor_json,next_cursor_json,chunk_count,total_bytes \
             FROM reporter_pending_pages WHERE account_id=?1 AND machine=?2 \
             AND destination_key=?3 AND kind=?4",
            params![
                account_id.get(),
                machine.as_str(),
                destination_key,
                kind.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    let Some((id, expected, next, chunk_count, total_bytes)) = row else {
        return Ok(None);
    };
    let mut statement = connection
        .prepare(
            "SELECT request_id,body,rows FROM reporter_pending_chunks \
             WHERE account_id=?1 AND pending_id=?2 ORDER BY chunk_index",
        )
        .map_err(sql_error)?;
    let raw_chunks = statement
        .query_map(params![account_id.get(), id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    let chunks = raw_chunks
        .into_iter()
        .map(|(request_id, body, rows)| {
            Ok(ReporterPendingChunk {
                request_id,
                body,
                rows: usize::try_from(rows).map_err(|_| {
                    PulseError::new(
                        PulseErrorKind::Storage,
                        "Pulse reporter outbox contains an invalid row count",
                    )
                })?,
            })
        })
        .collect::<PulseResult<Vec<_>>>()?;
    let expected_count = usize::try_from(chunk_count).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Pulse reporter outbox contains an invalid chunk count",
        )
    })?;
    let expected_bytes = usize::try_from(total_bytes).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Pulse reporter outbox contains an invalid byte count",
        )
    })?;
    if chunks.len() != expected_count
        || chunks.iter().map(|chunk| chunk.body.len()).sum::<usize>() != expected_bytes
    {
        return Err(PulseError::new(
            PulseErrorKind::Storage,
            "Pulse reporter outbox chunk manifest is inconsistent",
        ));
    }
    let draft = ReporterPendingDraft {
        kind,
        expected: decode(&expected)?,
        next: decode(&next)?,
        chunks,
    };
    draft.validate(account_id, machine)?;
    Ok(Some(ReporterPendingPage { id, draft }))
}

fn insert_sqlite_reporter_pending(
    transaction: &rusqlite::Transaction<'_>,
    account_id: AccountId,
    machine: &MachineName,
    destination_key: &str,
    draft: &ReporterPendingDraft,
) -> PulseResult<ReporterPendingPage> {
    let expected = encode(&draft.expected)?;
    let next = encode(&draft.next)?;
    let chunk_count = i64::try_from(draft.chunks.len())
        .map_err(|_| PulseError::invalid_input("too many reporter chunks"))?;
    let total_bytes = draft
        .chunks
        .iter()
        .map(|chunk| chunk.body.len())
        .sum::<usize>();
    let total_bytes = i64::try_from(total_bytes)
        .map_err(|_| PulseError::invalid_input("reporter outbox is too large"))?;
    transaction
        .execute(
            "INSERT INTO reporter_pending_pages \
             (account_id,machine,destination_key,kind,expected_cursor_json, \
              next_cursor_json,chunk_count,total_bytes) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                account_id.get(),
                machine.as_str(),
                destination_key,
                draft.kind.as_str(),
                expected,
                next,
                chunk_count,
                total_bytes
            ],
        )
        .map_err(sql_error)?;
    let pending_id = transaction.last_insert_rowid();
    for (index, chunk) in draft.chunks.iter().enumerate() {
        let index = i64::try_from(index)
            .map_err(|_| PulseError::invalid_input("too many reporter chunks"))?;
        let rows = i64::try_from(chunk.rows).map_err(|_| {
            PulseError::invalid_input("Pulse reporter chunk row count is too large")
        })?;
        transaction
            .execute(
                "INSERT INTO reporter_pending_chunks \
                 (pending_id,account_id,chunk_index,request_id,body,rows) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    pending_id,
                    account_id.get(),
                    index,
                    chunk.request_id,
                    chunk.body,
                    rows
                ],
            )
            .map_err(sql_error)?;
    }
    load_sqlite_reporter_pending(
        transaction,
        account_id,
        machine,
        destination_key,
        draft.kind,
    )?
    .ok_or_else(|| {
        PulseError::new(
            PulseErrorKind::Storage,
            "Pulse reporter outbox insert was not visible",
        )
    })
}

fn federation_query_limit(limit: usize) -> PulseResult<i64> {
    if limit == 0 || limit > usize::from(MAX_PAGE_ROWS).saturating_add(1) {
        return Err(PulseError::invalid_input(
            "Pulse federation export page limit is out of bounds",
        ));
    }
    i64::try_from(limit)
        .map_err(|_| PulseError::invalid_input("Pulse federation page limit is too large"))
}

fn export_position(
    phase: u8,
    values: impl IntoIterator<Item = String>,
) -> PulseResult<FederationExportPosition> {
    FederationExportPosition::new(phase, values.into_iter().collect())
}

#[expect(
    clippy::too_many_lines,
    reason = "the ordered SQL keyset phases must remain in one transaction and one visible sequence"
)]
fn sqlite_local_federation_page(
    connection: &Connection,
    account_id: AccountId,
    local_machine: &MachineName,
    after: Option<&FederationExportPosition>,
    limit: usize,
) -> PulseResult<Vec<LocalFederationRecord>> {
    let limit = federation_query_limit(limit)?;
    let after_phase = after.map_or(0, |position| position.phase);
    let mut records = Vec::new();

    if after.is_none() {
        let machine = connection
            .query_row(
                "SELECT name, first_seen_ms, last_seen_ms FROM machines \
                 WHERE account_id=?1 AND name=?2",
                params![account_id.get(), local_machine.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_error)?;
        if let Some((name, first_seen, last_seen)) = machine {
            records.push(LocalFederationRecord::new(
                export_position(0, [name.clone()])?,
                FederatedPulseRow::Machine(Machine {
                    account_id,
                    name: MachineName::new(name)?,
                    first_seen: instant(first_seen)?,
                    last_seen: instant(last_seen)?,
                }),
            )?);
        }
    }

    if records.len() < usize::try_from(limit).unwrap_or(usize::MAX) && after_phase <= 1 {
        let after_name = if after_phase == 1 {
            let values = &after.expect("phase has cursor").values;
            if values.len() != 1 {
                return Err(PulseError::invalid_input(
                    "Pulse federation profile cursor is invalid",
                ));
            }
            values[0].as_str()
        } else {
            ""
        };
        let remaining = limit.saturating_sub(i64::try_from(records.len()).unwrap_or(i64::MAX));
        let local_origin = encode(&ProfileOrigin::Local)?;
        let sql = format!(
            "SELECT {PROFILE_COLUMNS} FROM profiles WHERE account_id=?1 AND origin_json=?2 \
             AND name>?3 ORDER BY name LIMIT ?4"
        );
        let mut statement = connection.prepare(&sql).map_err(sql_error)?;
        let profiles = statement
            .query_map(
                params![account_id.get(), local_origin, after_name, remaining],
                raw_profile,
            )
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for raw in profiles {
            let mut profile = decode_profile(raw)?;
            let name = profile.name.as_str().to_owned();
            profile.config_dir = None;
            profile.api_key_env = None;
            profile.api_key_file = None;
            profile.refresh = RefreshPolicy::Never;
            profile.origin = ProfileOrigin::Reported;
            records.push(LocalFederationRecord::new(
                export_position(1, [name])?,
                FederatedPulseRow::Profile(profile),
            )?);
        }
    }

    if records.len() < usize::try_from(limit).unwrap_or(usize::MAX) && after_phase <= 2 {
        let after_id = if after_phase == 2 {
            let values = &after.expect("phase has cursor").values;
            if values.len() != 1 {
                return Err(PulseError::invalid_input(
                    "Pulse federation usage cursor is invalid",
                ));
            }
            values[0].parse::<i64>().map_err(|_| {
                PulseError::invalid_input("Pulse federation usage cursor is invalid")
            })?
        } else {
            0
        };
        let remaining = limit.saturating_sub(i64::try_from(records.len()).unwrap_or(i64::MAX));
        let mut statement = connection
            .prepare(
                "SELECT id, account_id, profile, machine, vendor_json, outcome_json, \
                 polled_at_ms, reporter_version FROM usage_snapshots \
                 WHERE account_id=?1 AND machine=?2 AND id>?3 ORDER BY id LIMIT ?4",
            )
            .map_err(sql_error)?;
        let snapshots = statement
            .query_map(
                params![
                    account_id.get(),
                    local_machine.as_str(),
                    after_id,
                    remaining
                ],
                raw_snapshot,
            )
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for raw in snapshots {
            let id = raw.id;
            let snapshot = decode_snapshot(connection, raw)?.snapshot;
            records.push(LocalFederationRecord::new(
                export_position(2, [id.to_string()])?,
                FederatedPulseRow::Usage(snapshot),
            )?);
        }
    }

    if records.len() < usize::try_from(limit).unwrap_or(usize::MAX) && after_phase <= 3 {
        let (after_profile, after_session) = if after_phase == 3 {
            let values = &after.expect("phase has cursor").values;
            if values.len() != 2 {
                return Err(PulseError::invalid_input(
                    "Pulse federation context cursor is invalid",
                ));
            }
            (values[0].as_str(), values[1].as_str())
        } else {
            ("", "")
        };
        let remaining = limit.saturating_sub(i64::try_from(records.len()).unwrap_or(i64::MAX));
        let mut statement = connection
            .prepare(
                "SELECT account_id, profile, machine, session_id, model, settings_json, \
                 context_tokens, context_percent, effective_limit, last_active_at_ms, \
                 last_reset_at_ms, collected_at_ms FROM context_sessions \
                 WHERE account_id=?1 AND machine=?2 AND (profile, session_id)>(?3,?4) \
                 ORDER BY profile, session_id LIMIT ?5",
            )
            .map_err(sql_error)?;
        let contexts = statement
            .query_map(
                params![
                    account_id.get(),
                    local_machine.as_str(),
                    after_profile,
                    after_session,
                    remaining
                ],
                raw_context,
            )
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for raw in contexts {
            let position = export_position(3, [raw.profile.clone(), raw.session_id.clone()])?;
            records.push(LocalFederationRecord::new(
                position,
                FederatedPulseRow::Context(decode_context(raw)?),
            )?);
        }
    }

    if records.len() < usize::try_from(limit).unwrap_or(usize::MAX) && after_phase <= 4 {
        let empty = [
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ];
        let values = if after_phase == 4 {
            let values = &after.expect("phase has cursor").values;
            if values.len() != 6 {
                return Err(PulseError::invalid_input(
                    "Pulse federation token cursor is invalid",
                ));
            }
            values.as_slice()
        } else {
            empty.as_slice()
        };
        let remaining = limit.saturating_sub(i64::try_from(records.len()).unwrap_or(i64::MAX));
        let mut statement = connection
            .prepare(
                "SELECT account_id, profile, machine, session_id, model, settings_hash, \
                 settings_json, day, tokens_in, tokens_out, cache_write_5m, cache_write_1h, \
                 cache_read, source_json FROM token_usage WHERE account_id=?1 AND machine=?2 \
                 AND (profile, session_id, model, settings_hash, day, source_json) \
                     >(?3,?4,?5,?6,?7,?8) \
                 ORDER BY profile, session_id, model, settings_hash, day, source_json LIMIT ?9",
            )
            .map_err(sql_error)?;
        let tokens = statement
            .query_map(
                params![
                    account_id.get(),
                    local_machine.as_str(),
                    &values[0],
                    &values[1],
                    &values[2],
                    &values[3],
                    &values[4],
                    &values[5],
                    remaining
                ],
                raw_token,
            )
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for raw in tokens {
            let position = export_position(
                4,
                [
                    raw.profile.clone(),
                    raw.session_id.clone(),
                    raw.model.clone(),
                    raw.settings_hash.clone(),
                    raw.day.clone(),
                    raw.source.clone(),
                ],
            )?;
            records.push(LocalFederationRecord::new(
                position,
                FederatedPulseRow::Token(decode_token(raw)?),
            )?);
        }
    }
    Ok(records)
}

impl Store for SqliteStore {
    fn schema_version(&self) -> StoreFuture<u32> {
        self.run(|connection| migrate::current_version(connection))
    }

    fn integrity_check(&self) -> StoreFuture<String> {
        self.run(|connection| {
            connection
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .map_err(sql_error)
        })
    }

    fn upsert_account(&self, account: Account) -> StoreFuture<()> {
        self.run(move |connection| {
            account.validate()?;
            connection
                .execute(
                    "INSERT INTO accounts (id, identity, display_name) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(id) DO UPDATE SET identity = excluded.identity, \
                     display_name = excluded.display_name",
                    params![account.id.get(), account.identity, account.display_name],
                )
                .map_err(sql_error)?;
            Ok(())
        })
    }

    fn get_account(&self, account_id: AccountId) -> StoreFuture<Option<Account>> {
        self.run(move |connection| {
            let raw = connection
                .query_row(
                    "SELECT id, identity, display_name FROM accounts WHERE id = ?1",
                    params![account_id.get()],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(sql_error)?;
            raw.map(|(id, identity, display_name)| {
                Ok(Account {
                    id: AccountId::new(id)?,
                    identity,
                    display_name,
                })
            })
            .transpose()
        })
    }

    fn upsert_machine(&self, machine: Machine) -> StoreFuture<()> {
        self.run(move |connection| {
            if machine.last_seen < machine.first_seen {
                return Err(PulseError::invalid_input(
                    "machine last_seen cannot precede first_seen",
                ));
            }
            connection
                .execute(
                    "INSERT INTO machines (account_id, name, first_seen_ms, last_seen_ms) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT(account_id, name) DO UPDATE SET \
                     first_seen_ms = MIN(machines.first_seen_ms, excluded.first_seen_ms), \
                     last_seen_ms = MAX(machines.last_seen_ms, excluded.last_seen_ms)",
                    params![
                        machine.account_id.get(),
                        machine.name.as_str(),
                        machine.first_seen.epoch_millis(),
                        machine.last_seen.epoch_millis()
                    ],
                )
                .map_err(sql_error)?;
            Ok(())
        })
    }

    fn list_machines(&self, account_id: AccountId) -> StoreFuture<Vec<Machine>> {
        self.run(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT name, first_seen_ms, last_seen_ms FROM machines \
                     WHERE account_id = ?1 ORDER BY name LIMIT 10001",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map(params![account_id.get()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            rows.into_iter()
                .map(|(name, first_seen, last_seen)| {
                    Ok(Machine {
                        account_id,
                        name: MachineName::new(name)?,
                        first_seen: instant(first_seen)?,
                        last_seen: instant(last_seen)?,
                    })
                })
                .collect()
        })
    }

    fn upsert_profile(&self, profile: Profile) -> StoreFuture<()> {
        self.run(move |connection| upsert_profile(connection, &profile))
    }

    fn get_profile(
        &self,
        account_id: AccountId,
        name: ProfileName,
    ) -> StoreFuture<Option<Profile>> {
        self.run(move |connection| {
            let sql = format!(
                "SELECT {PROFILE_COLUMNS} FROM profiles WHERE account_id = ?1 AND name = ?2"
            );
            connection
                .query_row(&sql, params![account_id.get(), name.as_str()], raw_profile)
                .optional()
                .map_err(sql_error)?
                .map(decode_profile)
                .transpose()
        })
    }

    fn list_profiles(&self, account_id: AccountId) -> StoreFuture<Vec<Profile>> {
        self.run(move |connection| {
            let sql = format!(
                "SELECT {PROFILE_COLUMNS} FROM profiles WHERE account_id = ?1 ORDER BY name LIMIT 10001"
            );
            let mut statement = connection.prepare(&sql).map_err(sql_error)?;
            statement
                .query_map(params![account_id.get()], raw_profile)
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
                .into_iter()
                .map(decode_profile)
                .collect()
        })
    }

    fn set_profile_hidden(
        &self,
        account_id: AccountId,
        name: ProfileName,
        hidden: bool,
    ) -> StoreFuture<bool> {
        self.run(move |connection| {
            connection
                .execute(
                    "UPDATE profiles SET hidden = ?3 WHERE account_id = ?1 AND name = ?2",
                    params![account_id.get(), name.as_str(), i64::from(hidden)],
                )
                .map(|count| count != 0)
                .map_err(sql_error)
        })
    }

    fn delete_profile(&self, account_id: AccountId, name: ProfileName) -> StoreFuture<bool> {
        self.run(move |connection| {
            connection
                .execute(
                    "DELETE FROM profiles WHERE account_id = ?1 AND name = ?2",
                    params![account_id.get(), name.as_str()],
                )
                .map(|count| count != 0)
                .map_err(sql_error)
        })
    }

    fn append_usage_snapshot(&self, snapshot: UsageSnapshot) -> StoreFuture<i64> {
        self.run(move |connection| append_snapshot(connection, &snapshot))
    }

    fn usage_history(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        since: Option<Instant>,
        limit: usize,
    ) -> StoreFuture<Vec<StoredUsageSnapshot>> {
        self.run(move |connection| {
            let limit = query_limit(limit)?;
            let since = since.map_or(i64::MIN, Instant::epoch_millis);
            let mut statement = connection
                .prepare(
                    "SELECT id, account_id, profile, machine, vendor_json, outcome_json, \
                     polled_at_ms, reporter_version FROM usage_snapshots \
                     WHERE account_id = ?1 AND profile = ?2 AND polled_at_ms >= ?3 \
                     ORDER BY polled_at_ms DESC, id DESC LIMIT ?4",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map(
                    params![account_id.get(), profile.as_str(), since, limit],
                    raw_snapshot,
                )
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            rows.into_iter()
                .map(|row| decode_snapshot(connection, row))
                .collect()
        })
    }

    fn current_usage(
        &self,
        account_id: AccountId,
        profile: ProfileName,
    ) -> StoreFuture<Vec<CurrentQuotaWindow>> {
        self.run(move |connection| load_current_usage(connection, account_id, &profile))
    }

    fn upsert_context_session(&self, session: ContextSession) -> StoreFuture<()> {
        self.run(move |connection| upsert_context(connection, &session))
    }

    fn list_context_sessions(
        &self,
        account_id: AccountId,
        profile: Option<ProfileName>,
    ) -> StoreFuture<Vec<ContextSession>> {
        self.run(move |connection| {
            let mut sql = String::from(
                "SELECT account_id, profile, machine, session_id, model, settings_json, \
                 context_tokens, context_percent, effective_limit, last_active_at_ms, \
                 last_reset_at_ms, collected_at_ms FROM context_sessions WHERE account_id = ?1",
            );
            if profile.is_some() {
                sql.push_str(" AND profile = ?2");
            }
            sql.push_str(
                " ORDER BY last_active_at_ms DESC, profile, machine, session_id LIMIT 10001",
            );
            let mut statement = connection.prepare(&sql).map_err(sql_error)?;
            let rows = if let Some(profile) = profile {
                statement
                    .query_map(params![account_id.get(), profile.as_str()], raw_context)
                    .map_err(sql_error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_error)?
            } else {
                statement
                    .query_map(params![account_id.get()], raw_context)
                    .map_err(sql_error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_error)?
            };
            rows.into_iter().map(decode_context).collect()
        })
    }

    fn upsert_token_grain(&self, grain: TokenGrain) -> StoreFuture<()> {
        self.run(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            upsert_token_batch(&transaction, std::iter::once(&grain))?;
            transaction.commit().map_err(sql_error)
        })
    }

    fn begin_token_observation(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        machine: MachineName,
    ) -> StoreFuture<TokenWriteObservation> {
        self.run(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let revision =
                allocate_sqlite_token_revision(&transaction, account_id, &profile, &machine)?;
            transaction.commit().map_err(sql_error)?;
            Ok(TokenWriteObservation {
                account_id,
                profile,
                machine,
                revision: as_u64(revision)?,
            })
        })
    }

    fn upsert_observed_token_grain(
        &self,
        observation: TokenWriteObservation,
        grain: TokenGrain,
    ) -> StoreFuture<()> {
        self.run(move |connection| {
            validate_token_observation(&observation, &grain)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let reserved = sqlite_token_write_revision(
                &transaction,
                observation.account_id,
                &observation.profile,
                &observation.machine,
            )?;
            let revision = i64::try_from(observation.revision).map_err(|_| {
                PulseError::new(
                    PulseErrorKind::Storage,
                    "token observation revision overflowed",
                )
            })?;
            if reserved != revision {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse token observation is no longer current",
                ));
            }
            upsert_token_at_revision(&transaction, &grain, revision, true)?;
            transaction.commit().map_err(sql_error)
        })
    }

    fn list_token_grains(
        &self,
        account_id: AccountId,
        profile: Option<ProfileName>,
        since_day: Option<String>,
        limit: usize,
    ) -> StoreFuture<Vec<TokenGrain>> {
        self.run(move |connection| {
            let limit = query_limit(limit)?;
            if let Some(day) = &since_day {
                jiff::civil::Date::from_str(day).map_err(|error| {
                    PulseError::invalid_input(format!("invalid since_day: {error}"))
                })?;
            }
            let since_day = since_day.unwrap_or_else(|| "0000-01-01".to_owned());
            let mut sql = String::from(
                "SELECT account_id, profile, machine, session_id, model, settings_hash, \
                 settings_json, day, tokens_in, tokens_out, cache_write_5m, cache_write_1h, \
                 cache_read, source_json FROM token_usage \
                 WHERE account_id = ?1 AND day >= ?2",
            );
            if profile.is_some() {
                sql.push_str(
                    " AND profile = ?3 ORDER BY day DESC, profile, machine, session_id LIMIT ?4",
                );
            } else {
                sql.push_str(" ORDER BY day DESC, profile, machine, session_id LIMIT ?3");
            }
            let mut statement = connection.prepare(&sql).map_err(sql_error)?;
            let rows = if let Some(profile) = profile {
                statement
                    .query_map(
                        params![account_id.get(), since_day, profile.as_str(), limit],
                        raw_token,
                    )
                    .map_err(sql_error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_error)?
            } else {
                statement
                    .query_map(params![account_id.get(), since_day, limit], raw_token)
                    .map_err(sql_error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_error)?
            };
            rows.into_iter().map(decode_token).collect()
        })
    }

    fn token_totals_by_keys(
        &self,
        account_id: AccountId,
        keys: Vec<TokenReconciliationKey>,
    ) -> StoreFuture<Vec<(TokenReconciliationKey, StoredTokenTotals)>> {
        self.run(move |connection| {
            validate_reconciliation_keys(&keys)?;
            let mut totals = Vec::with_capacity(keys.len());
            let mut statement = connection
                .prepare(
                    "SELECT COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), \
                     COALESCE(SUM(cache_write_5m),0), COALESCE(SUM(cache_write_1h),0), \
                     COALESCE(SUM(cache_read),0) FROM token_usage \
                     WHERE account_id=?1 AND profile=?2 AND day=?3",
                )
                .map_err(sql_error)?;
            for key in keys {
                let raw = statement
                    .query_row(
                        params![account_id.get(), key.profile.as_str(), key.day],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, i64>(4)?,
                            ))
                        },
                    )
                    .map_err(sql_error)?;
                totals.push((
                    key,
                    StoredTokenTotals {
                        tokens_in: nonnegative_total(raw.0)?,
                        tokens_out: nonnegative_total(raw.1)?,
                        cache_write_5m: nonnegative_total(raw.2)?,
                        cache_write_1h: nonnegative_total(raw.3)?,
                        cache_read: nonnegative_total(raw.4)?,
                    },
                ));
            }
            Ok(totals)
        })
    }

    fn upsert_pricing_default(&self, rule: PricingRule) -> StoreFuture<()> {
        self.run(move |connection| upsert_pricing_default(connection, &rule))
    }

    fn upsert_pricing_override(&self, account_id: AccountId, rule: PricingRule) -> StoreFuture<()> {
        self.run(move |connection| upsert_pricing_override(connection, account_id, &rule))
    }

    fn delete_pricing_override(&self, account_id: AccountId, key: String) -> StoreFuture<bool> {
        self.run(move |connection| {
            validate_pricing_key(&key)?;
            connection
                .execute(
                    "DELETE FROM pricing_overrides WHERE account_id = ?1 AND key = ?2",
                    params![account_id.get(), key],
                )
                .map(|deleted| deleted == 1)
                .map_err(sql_error)
        })
    }

    fn list_pricing_defaults(&self) -> StoreFuture<Vec<PricingRule>> {
        self.run(|connection| {
            let mut statement = connection
                .prepare(
                    "SELECT key, vendor_json, model_pattern, settings_json, input_rate, \
                     output_rate, cache_write_5m_rate, cache_write_1h_rate, cache_read_rate \
                     FROM pricing_defaults ORDER BY key LIMIT 10001",
                )
                .map_err(sql_error)?;
            statement
                .query_map([], raw_pricing)
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
                .into_iter()
                .map(decode_pricing)
                .collect()
        })
    }

    fn list_pricing_overrides(&self, account_id: AccountId) -> StoreFuture<Vec<PricingRule>> {
        self.run(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT key, vendor_json, model_pattern, settings_json, input_rate, \
                     output_rate, cache_write_5m_rate, cache_write_1h_rate, cache_read_rate \
                     FROM pricing_overrides WHERE account_id = ?1 ORDER BY key LIMIT 10001",
                )
                .map_err(sql_error)?;
            statement
                .query_map(params![account_id.get()], raw_pricing)
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
                .into_iter()
                .map(decode_pricing)
                .collect()
        })
    }

    fn create_alert_subscription(
        &self,
        subscription: AlertSubscription,
        created_at: Instant,
    ) -> StoreFuture<StoredAlertSubscription> {
        self.run(move |connection| {
            subscription.validate()?;
            let alert_type = encode(&subscription.alert_type)?;
            let delivery = subscription.delivery.as_ref().map(encode).transpose()?;
            let threshold = subscription.threshold.map(Percent::get);
            let threshold_key = threshold.map_or_else(
                || "none".to_owned(),
                |value| format!("{value:.9}"),
            );
            connection
                .execute(
                    "INSERT INTO alert_subscriptions (account_id, profile, alert_type_json, \
                     threshold, threshold_key, cooldown_minutes, delivery_json, enabled, \
                     created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                     ON CONFLICT(account_id, profile, alert_type_json, threshold_key) DO UPDATE SET \
                     cooldown_minutes = excluded.cooldown_minutes, \
                     delivery_json = excluded.delivery_json, enabled = excluded.enabled",
                    params![
                        subscription.account_id.get(),
                        subscription.profile.as_str(),
                        alert_type,
                        threshold,
                        threshold_key,
                        subscription.cooldown_minutes,
                        delivery,
                        i64::from(subscription.enabled),
                        created_at.epoch_millis()
                    ],
                )
                .map_err(sql_error)?;
            let id = connection
                .query_row(
                    "SELECT id FROM alert_subscriptions WHERE account_id = ?1 AND profile = ?2 \
                     AND alert_type_json = ?3 AND threshold_key = ?4",
                    params![
                        subscription.account_id.get(),
                        subscription.profile.as_str(),
                        encode(&subscription.alert_type)?,
                        threshold_key
                    ],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            Ok(StoredAlertSubscription {
                id,
                subscription,
                created_at,
            })
        })
    }

    fn list_alert_subscriptions(
        &self,
        account_id: AccountId,
    ) -> StoreFuture<Vec<StoredAlertSubscription>> {
        self.run(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id, account_id, profile, alert_type_json, threshold, \
                     cooldown_minutes, delivery_json, enabled, created_at_ms \
                     FROM alert_subscriptions WHERE account_id = ?1 ORDER BY id LIMIT 10001",
                )
                .map_err(sql_error)?;
            statement
                .query_map(params![account_id.get()], raw_alert_subscription)
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
                .into_iter()
                .map(decode_alert_subscription)
                .collect()
        })
    }

    fn delete_alert_subscription(
        &self,
        account_id: AccountId,
        subscription_id: i64,
    ) -> StoreFuture<bool> {
        self.run(move |connection| {
            connection
                .execute(
                    "DELETE FROM alert_subscriptions WHERE account_id = ?1 AND id = ?2",
                    params![account_id.get(), subscription_id],
                )
                .map(|count| count != 0)
                .map_err(sql_error)
        })
    }

    fn record_alert_if_due(&self, event: AlertEventInput) -> StoreFuture<Option<AlertEvent>> {
        self.run(move |connection| record_alert_if_due(connection, &event))
    }

    fn list_alert_events(
        &self,
        account_id: AccountId,
        acknowledged: Option<bool>,
    ) -> StoreFuture<Vec<AlertEvent>> {
        self.run(move |connection| {
            let (sql, acknowledged_value) = if let Some(value) = acknowledged {
                (
                    "SELECT id, account_id, subscription_id, profile, alert_type_json, \
                     message, current_value, threshold, acknowledged, triggered_at_ms \
                     FROM alert_events WHERE account_id = ?1 AND acknowledged = ?2 \
                     ORDER BY triggered_at_ms DESC, id DESC LIMIT 10001",
                    Some(i64::from(value)),
                )
            } else {
                (
                    "SELECT id, account_id, subscription_id, profile, alert_type_json, \
                     message, current_value, threshold, acknowledged, triggered_at_ms \
                     FROM alert_events WHERE account_id = ?1 \
                     ORDER BY triggered_at_ms DESC, id DESC LIMIT 10001",
                    None,
                )
            };
            let mut statement = connection.prepare(sql).map_err(sql_error)?;
            let rows = if let Some(value) = acknowledged_value {
                statement
                    .query_map(params![account_id.get(), value], raw_alert_event)
                    .map_err(sql_error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_error)?
            } else {
                statement
                    .query_map(params![account_id.get()], raw_alert_event)
                    .map_err(sql_error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sql_error)?
            };
            rows.into_iter().map(decode_alert_event).collect()
        })
    }

    fn acknowledge_alert(&self, account_id: AccountId, event_id: i64) -> StoreFuture<bool> {
        self.run(move |connection| {
            connection
                .execute(
                    "UPDATE alert_events SET acknowledged = 1 WHERE account_id = ?1 AND id = ?2",
                    params![account_id.get(), event_id],
                )
                .map(|count| count != 0)
                .map_err(sql_error)
        })
    }

    fn reply_to_alert(&self, reply: AlertReplyInput) -> StoreFuture<Option<AlertReply>> {
        self.run(move |connection| reply_to_alert(connection, &reply))
    }

    fn list_alert_replies(
        &self,
        account_id: AccountId,
        event_id: i64,
    ) -> StoreFuture<Vec<AlertReply>> {
        self.run(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id, account_id, event_id, message, replied_at_ms FROM alert_replies \
                     WHERE account_id = ?1 AND event_id = ?2 ORDER BY replied_at_ms, id LIMIT 256",
                )
                .map_err(sql_error)?;
            statement
                .query_map(params![account_id.get(), event_id], raw_alert_reply)
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
                .into_iter()
                .map(decode_alert_reply)
                .collect()
        })
    }

    fn schedule_reset_resume(
        &self,
        input: ResetResumeInput,
        limits: ResetResumeLimits,
    ) -> StoreFuture<ResetResumeJob> {
        self.run(move |connection| schedule_reset_resume(connection, &input, limits))
    }

    fn list_pending_reset_resumes(
        &self,
        account_id: AccountId,
        through: Instant,
        limit: usize,
    ) -> StoreFuture<Vec<ResetResumeJob>> {
        self.run(move |connection| {
            let limit = query_limit(limit)?;
            let mut statement = connection
                .prepare(
                    "SELECT id, account_id, profile, resets_at_ms, resume_at_ms, scheduled_at_ms, \
                     lease_until_ms, attempts, delivered_at_ms, cancelled_at_ms \
                     FROM reset_resume_jobs WHERE account_id=?1 AND resume_at_ms<=?2 \
                     AND delivered_at_ms IS NULL AND cancelled_at_ms IS NULL \
                     ORDER BY resume_at_ms, id LIMIT ?3",
                )
                .map_err(sql_error)?;
            statement
                .query_map(
                    params![account_id.get(), through.epoch_millis(), limit],
                    raw_reset_resume,
                )
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
                .into_iter()
                .map(decode_reset_resume)
                .collect()
        })
    }

    fn claim_due_reset_resumes(
        &self,
        account_id: AccountId,
        now: Instant,
        lease_until: Instant,
        limit: usize,
    ) -> StoreFuture<Vec<ResetResumeJob>> {
        self.run(move |connection| {
            claim_due_reset_resumes(connection, account_id, now, lease_until, limit)
        })
    }

    fn complete_reset_resume(
        &self,
        account_id: AccountId,
        job_id: i64,
        delivered_at: Instant,
    ) -> StoreFuture<bool> {
        self.run(move |connection| {
            connection
                .execute(
                    "UPDATE reset_resume_jobs SET delivered_at_ms=?3, lease_until_ms=NULL \
                     WHERE account_id=?1 AND id=?2 AND delivered_at_ms IS NULL \
                     AND cancelled_at_ms IS NULL",
                    params![account_id.get(), job_id, delivered_at.epoch_millis()],
                )
                .map(|count| count != 0)
                .map_err(sql_error)
        })
    }

    fn cancel_reset_resumes(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        cancelled_at: Instant,
    ) -> StoreFuture<usize> {
        self.run(move |connection| {
            connection
                .execute(
                    "UPDATE reset_resume_jobs SET cancelled_at_ms=?3, lease_until_ms=NULL \
                     WHERE account_id=?1 AND profile=?2 AND delivered_at_ms IS NULL \
                     AND cancelled_at_ms IS NULL",
                    params![
                        account_id.get(),
                        profile.as_str(),
                        cancelled_at.epoch_millis()
                    ],
                )
                .map_err(sql_error)
        })
    }

    fn insert_ingest_token(&self, token: IngestToken) -> StoreFuture<()> {
        self.run(move |connection| {
            validate_token_hash(&token.token_hash)?;
            if token.id <= 0 {
                return Err(PulseError::invalid_input(
                    "ingest token id must be positive",
                ));
            }
            connection
                .execute(
                    "INSERT INTO ingest_tokens (id, account_id, machine, token_hash, \
                     created_at_ms, last_used_at_ms, revoked_at_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        token.id,
                        token.account_id.get(),
                        token.machine.as_str(),
                        token.token_hash,
                        token.created_at.epoch_millis(),
                        token.last_used_at.map(Instant::epoch_millis),
                        token.revoked_at.map(Instant::epoch_millis)
                    ],
                )
                .map_err(sql_error)?;
            Ok(())
        })
    }

    fn issue_ingest_token(
        &self,
        machine: Machine,
        token: IngestToken,
        max_active_tokens: usize,
    ) -> StoreFuture<()> {
        self.run(move |connection| {
            validate_issued_token(&machine, &token, max_active_tokens)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let active = transaction
                .query_row(
                    "SELECT COUNT(*) FROM ingest_tokens \
                     WHERE account_id=?1 AND revoked_at_ms IS NULL",
                    params![token.account_id.get()],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(sql_error)?;
            if usize::try_from(active).unwrap_or(usize::MAX) >= max_active_tokens {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse ingest tokens reached the account cap",
                ));
            }
            transaction
                .execute(
                    "INSERT INTO machines (account_id, name, first_seen_ms, last_seen_ms) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT(account_id, name) DO UPDATE SET \
                     first_seen_ms=MIN(machines.first_seen_ms, excluded.first_seen_ms), \
                     last_seen_ms=MAX(machines.last_seen_ms, excluded.last_seen_ms)",
                    params![
                        machine.account_id.get(),
                        machine.name.as_str(),
                        machine.first_seen.epoch_millis(),
                        machine.last_seen.epoch_millis()
                    ],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "INSERT INTO ingest_tokens (id, account_id, machine, token_hash, \
                     created_at_ms, last_used_at_ms, revoked_at_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL)",
                    params![
                        token.id,
                        token.account_id.get(),
                        token.machine.as_str(),
                        token.token_hash,
                        token.created_at.epoch_millis()
                    ],
                )
                .map_err(sql_error)?;
            transaction.commit().map_err(sql_error)
        })
    }

    fn list_ingest_tokens(&self, account_id: AccountId) -> StoreFuture<Vec<IngestToken>> {
        self.run(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id, account_id, machine, token_hash, created_at_ms, \
                     last_used_at_ms, revoked_at_ms FROM ingest_tokens \
                     WHERE account_id = ?1 ORDER BY id",
                )
                .map_err(sql_error)?;
            statement
                .query_map(params![account_id.get()], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                    ))
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
                .into_iter()
                .map(|(id, account, machine, hash, created, used, revoked)| {
                    Ok(IngestToken {
                        id,
                        account_id: AccountId::new(account)?,
                        machine: MachineName::new(machine)?,
                        token_hash: hash,
                        created_at: instant(created)?,
                        last_used_at: used.map(instant).transpose()?,
                        revoked_at: revoked.map(instant).transpose()?,
                    })
                })
                .collect()
        })
    }

    fn get_ingest_token(
        &self,
        account_id: AccountId,
        token_id: i64,
    ) -> StoreFuture<Option<IngestToken>> {
        self.run(move |connection| {
            connection
                .query_row(
                    "SELECT id, account_id, machine, token_hash, created_at_ms, last_used_at_ms, \
                     revoked_at_ms FROM ingest_tokens WHERE account_id=?1 AND id=?2",
                    params![account_id.get(), token_id],
                    raw_ingest_token,
                )
                .optional()
                .map_err(sql_error)?
                .map(decode_ingest_token)
                .transpose()
        })
    }

    fn touch_ingest_token(
        &self,
        account_id: AccountId,
        token_id: i64,
        used_at: Instant,
    ) -> StoreFuture<bool> {
        self.run(move |connection| {
            connection
                .execute(
                    "UPDATE ingest_tokens SET last_used_at_ms = MAX(COALESCE(last_used_at_ms, ?3), ?3) \
                     WHERE account_id = ?1 AND id = ?2 AND revoked_at_ms IS NULL",
                    params![account_id.get(), token_id, used_at.epoch_millis()],
                )
                .map(|count| count != 0)
                .map_err(sql_error)
        })
    }

    fn revoke_ingest_token(
        &self,
        account_id: AccountId,
        token_id: i64,
        revoked_at: Instant,
    ) -> StoreFuture<bool> {
        self.run(move |connection| {
            connection
                .execute(
                    "UPDATE ingest_tokens SET revoked_at_ms = COALESCE(revoked_at_ms, ?3) \
                     WHERE account_id = ?1 AND id = ?2",
                    params![account_id.get(), token_id, revoked_at.epoch_millis()],
                )
                .map(|count| count != 0)
                .map_err(sql_error)
        })
    }

    fn upsert_gemini_quota(&self, quota: GeminiQuota) -> StoreFuture<()> {
        self.run(move |connection| upsert_gemini(connection, &quota))
    }

    fn list_gemini_quotas(&self, account_id: AccountId) -> StoreFuture<Vec<GeminiQuota>> {
        self.run(move |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT account_id, model_id, remaining_fraction, remaining_amount, \
                     resets_at_ms, collected_at_ms FROM gemini_quota \
                     WHERE account_id = ?1 ORDER BY model_id LIMIT 10001",
                )
                .map_err(sql_error)?;
            statement
                .query_map(params![account_id.get()], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
                .into_iter()
                .map(|(account, model, fraction, amount, reset, collected)| {
                    Ok(GeminiQuota {
                        account_id: AccountId::new(account)?,
                        model_id: model,
                        remaining_fraction: Fraction::new(fraction)?,
                        remaining_amount: amount,
                        resets_at: reset.map(instant).transpose()?,
                        collected_at: instant(collected)?,
                    })
                })
                .collect()
        })
    }

    fn record_import(&self, provenance: ImportProvenance) -> StoreFuture<bool> {
        self.run(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let inserted = claim_import(&transaction, &provenance)?;
            transaction.commit().map_err(sql_error)?;
            Ok(inserted)
        })
    }

    fn append_imported_usage_snapshot_once(
        &self,
        provenance: ImportProvenance,
        snapshot: UsageSnapshot,
    ) -> StoreFuture<bool> {
        self.run(move |connection| {
            snapshot.validate()?;
            if provenance.account_id != snapshot.account_id {
                return Err(PulseError::invalid_input(
                    "Pulse import provenance and snapshot accounts differ",
                ));
            }
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let inserted = claim_import(&transaction, &provenance)?;
            if inserted {
                insert_snapshot(&transaction, &snapshot)?;
            }
            transaction.commit().map_err(sql_error)?;
            Ok(inserted)
        })
    }

    fn apply_import_batch_once(&self, batch: ImportBatch) -> StoreFuture<ImportBatchResult> {
        self.run(move |connection| {
            validate_import_batch(&batch)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let mut result = ImportBatchResult::default();

            for machine in &batch.prerequisite_machines {
                upsert_import_machine(&transaction, machine)?;
            }
            for row in &batch.profiles {
                let inserted = claim_import(&transaction, &row.provenance)?;
                if inserted {
                    upsert_profile(&transaction, &row.value)?;
                }
                result.profiles.push(inserted);
            }
            for row in &batch.machines {
                let inserted = claim_import(&transaction, &row.provenance)?;
                if inserted {
                    upsert_import_machine(&transaction, &row.value)?;
                }
                result.machines.push(inserted);
            }
            for row in &batch.snapshots {
                let inserted = claim_import(&transaction, &row.provenance)?;
                if inserted {
                    insert_snapshot(&transaction, &row.value)?;
                }
                result.snapshots.push(inserted);
            }
            let mut token_writes = Vec::new();
            for row in &batch.token_grains {
                let inserted = claim_import(&transaction, &row.provenance)?;
                if inserted {
                    token_writes.push(&row.value);
                }
                result.token_grains.push(inserted);
            }
            upsert_token_batch(&transaction, token_writes)?;
            for row in &batch.context_sessions {
                let inserted = claim_import(&transaction, &row.provenance)?;
                if inserted {
                    upsert_context(&transaction, &row.value)?;
                }
                result.context_sessions.push(inserted);
            }
            for row in &batch.gemini_quotas {
                let inserted = claim_import(&transaction, &row.provenance)?;
                if inserted {
                    upsert_gemini(&transaction, &row.value)?;
                }
                result.gemini_quotas.push(inserted);
            }
            for row in &batch.pricing_overrides {
                let inserted = claim_import(&transaction, &row.provenance)?;
                if inserted {
                    upsert_pricing_override(&transaction, batch.account_id, &row.value)?;
                }
                result.pricing_overrides.push(inserted);
            }
            for row in &batch.alert_subscriptions {
                let inserted = claim_import(&transaction, &row.provenance)?;
                if inserted {
                    upsert_import_alert_subscription(&transaction, &row.value)?;
                }
                result.alert_subscriptions.push(inserted);
            }
            for row in &batch.alert_events {
                let inserted = claim_import(&transaction, &row.provenance)?;
                if inserted {
                    insert_import_alert_event(&transaction, &row.value)?;
                }
                result.alert_events.push(inserted);
            }
            transaction.commit().map_err(sql_error)?;
            Ok(result)
        })
    }

    fn begin_token_backfill(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        machine: MachineName,
        source_generation: TokenSourceGeneration,
        restart_completed: bool,
    ) -> StoreFuture<TokenBackfillState> {
        self.run(move |connection| {
            begin_sqlite_token_backfill(
                connection,
                account_id,
                &profile,
                &machine,
                &source_generation,
                restart_completed,
            )
        })
    }

    fn apply_token_backfill_page(
        &self,
        page: TokenBackfillPage,
    ) -> StoreFuture<TokenBackfillState> {
        self.run(move |connection| apply_sqlite_token_backfill_page(connection, &page))
    }

    fn begin_federation_sync(
        &self,
        account_id: AccountId,
        source_machine: MachineName,
    ) -> StoreFuture<FederationState> {
        self.run(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            transaction
                .execute(
                    "INSERT INTO federation_peers (account_id, source_machine) VALUES (?1, ?2) \
                     ON CONFLICT(account_id, source_machine) DO NOTHING",
                    params![account_id.get(), source_machine.as_str()],
                )
                .map_err(sql_error)?;
            let mut state = load_federation_state(&transaction, account_id, &source_machine)?;
            if state.complete {
                transaction
                    .execute(
                        "UPDATE federation_peers SET cursor=NULL, generation=generation+1, \
                         complete=0 WHERE account_id=?1 AND source_machine=?2",
                        params![account_id.get(), source_machine.as_str()],
                    )
                    .map_err(sql_error)?;
                state.cursor = None;
                state.generation = state.generation.saturating_add(1);
                state.complete = false;
            }
            transaction.commit().map_err(sql_error)?;
            Ok(state)
        })
    }

    fn apply_federation_page(
        &self,
        account_id: AccountId,
        source_machine: MachineName,
        expected_cursor: Option<OpaqueCursor>,
        next_cursor: Option<OpaqueCursor>,
        mut records: Vec<FederatedRecord>,
    ) -> StoreFuture<FederationState> {
        self.run(move |connection| {
            let mut keys = HashSet::with_capacity(records.len());
            for record in &records {
                record.validate(account_id, &source_machine)?;
                if !keys.insert(record.key.clone()) {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse federation page repeated a record key",
                    ));
                }
            }
            records.sort_by_key(FederatedRecord::apply_priority);
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let state = load_federation_state(&transaction, account_id, &source_machine)?;
            if state.cursor != expected_cursor || state.complete {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse federation cursor no longer matches durable state",
                ));
            }

            let mut pending = Vec::new();
            for record in records {
                let fingerprint = record.fingerprint()?;
                let existing = transaction
                    .query_row(
                        "SELECT fingerprint FROM federation_records WHERE account_id=?1 \
                         AND source_machine=?2 AND record_key=?3",
                        params![account_id.get(), source_machine.as_str(), record.key],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(sql_error)?;
                match existing {
                    Some(existing) if existing == fingerprint => {}
                    Some(_) => {
                        return Err(PulseError::new(
                            PulseErrorKind::Conflict,
                            "Pulse federation stable key changed its fingerprint",
                        ));
                    }
                    None => pending.push((record, fingerprint)),
                }
            }

            for (record, _) in &pending {
                if !matches!(record.row, FederatedPulseRow::Token(_)) {
                    apply_sqlite_federated_row(&transaction, &record.row)?;
                }
            }
            upsert_token_batch(
                &transaction,
                pending.iter().filter_map(|(record, _)| match &record.row {
                    FederatedPulseRow::Token(grain) => Some(grain),
                    _ => None,
                }),
            )?;
            for (record, fingerprint) in &pending {
                transaction
                    .execute(
                        "INSERT INTO federation_records (account_id, source_machine, record_key, \
                         fingerprint, received_at_ms) VALUES (?1,?2,?3,?4,?5)",
                        params![
                            account_id.get(),
                            source_machine.as_str(),
                            record.key,
                            fingerprint,
                            Instant::now().epoch_millis()
                        ],
                    )
                    .map_err(sql_error)?;
            }
            let inserted = i64::try_from(pending.len()).map_err(|_| {
                PulseError::new(PulseErrorKind::Internal, "federation page count overflow")
            })?;
            transaction
                .execute(
                    "UPDATE federation_peers SET cursor=?3, pages_applied=pages_applied+1, \
                     records_applied=records_applied+?4, complete=?5 \
                     WHERE account_id=?1 AND source_machine=?2",
                    params![
                        account_id.get(),
                        source_machine.as_str(),
                        next_cursor.as_ref().map(OpaqueCursor::as_str),
                        inserted,
                        i64::from(next_cursor.is_none())
                    ],
                )
                .map_err(sql_error)?;
            let state = load_federation_state(&transaction, account_id, &source_machine)?;
            transaction.commit().map_err(sql_error)?;
            Ok(state)
        })
    }

    fn local_federation_page(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        after: Option<FederationExportPosition>,
        limit: usize,
    ) -> StoreFuture<Vec<LocalFederationRecord>> {
        self.run(move |connection| {
            sqlite_local_federation_page(
                connection,
                account_id,
                &local_machine,
                after.as_ref(),
                limit,
            )
        })
    }

    fn load_reporter_cursor(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
    ) -> StoreFuture<ReporterCursorState> {
        self.run(move |connection| {
            validate_reporter_destination(&destination_key)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM reporter_cursors WHERE account_id=?1 AND machine=?2 \
                     AND destination_key=?3",
                    params![account_id.get(), local_machine.as_str(), destination_key],
                    |_| Ok(()),
                )
                .optional()
                .map_err(sql_error)?
                .is_some();
            if !exists {
                let destinations = transaction
                    .query_row(
                        "SELECT COUNT(*) FROM reporter_cursors WHERE account_id=?1",
                        params![account_id.get()],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(sql_error)?;
                let maximum =
                    i64::try_from(MAX_REPORTER_DESTINATIONS_PER_ACCOUNT).map_err(|_| {
                        PulseError::new(
                            PulseErrorKind::Internal,
                            "Pulse reporter destination bound is invalid",
                        )
                    })?;
                if destinations >= maximum {
                    return Err(PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse reporter destination limit was reached for this account",
                    ));
                }
                transaction
                    .execute(
                        "INSERT INTO reporter_cursors (account_id,machine,destination_key) \
                         VALUES (?1,?2,?3)",
                        params![account_id.get(), local_machine.as_str(), destination_key],
                    )
                    .map_err(sql_error)?;
            }
            let state = transaction
                .query_row(
                    "SELECT usage_after_id,token_cursor_json,token_generation \
                     FROM reporter_cursors WHERE account_id=?1 AND machine=?2 \
                     AND destination_key=?3",
                    params![account_id.get(), local_machine.as_str(), destination_key],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .map_err(sql_error)?;
            let state = ReporterCursorState {
                usage_after_id: state.0,
                token_after: state.1.as_deref().map(decode).transpose()?,
                token_generation: as_u64(state.2)?,
            };
            transaction.commit().map_err(sql_error)?;
            Ok(state)
        })
    }

    fn local_reporter_usage_page(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        after_id: i64,
        limit: usize,
    ) -> StoreFuture<Vec<StoredUsageSnapshot>> {
        self.run(move |connection| {
            if after_id < 0 {
                return Err(PulseError::invalid_input(
                    "Pulse reporter usage cursor is invalid",
                ));
            }
            let limit = query_limit(limit)?;
            let mut statement = connection
                .prepare(
                    "SELECT id,account_id,profile,machine,vendor_json,outcome_json, \
                     polled_at_ms,reporter_version FROM usage_snapshots \
                     WHERE account_id=?1 AND machine=?2 AND id>?3 ORDER BY id LIMIT ?4",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map(
                    params![account_id.get(), local_machine.as_str(), after_id, limit],
                    raw_snapshot,
                )
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            rows.into_iter()
                .map(|row| decode_snapshot(connection, row))
                .collect()
        })
    }

    fn local_reporter_token_page(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        after: Option<ReporterTokenPosition>,
        limit: usize,
    ) -> StoreFuture<Vec<TokenGrain>> {
        self.run(move |connection| {
            let limit = query_limit(limit)?;
            let mut statement = connection
                .prepare(
                    "SELECT account_id,profile,machine,session_id,model,settings_hash, \
                     settings_json,day,tokens_in,tokens_out,cache_write_5m,cache_write_1h, \
                     cache_read,source_json FROM token_usage WHERE account_id=?1 AND machine=?2 \
                     AND (?3=0 OR (profile,session_id,model,settings_hash,day,source_json) \
                         > (?4,?5,?6,?7,?8,?9)) \
                     ORDER BY profile,session_id,model,settings_hash,day,source_json LIMIT ?10",
                )
                .map_err(sql_error)?;
            let (present, profile, session, model, settings, day, source) = after.map_or_else(
                || {
                    (
                        0,
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                    )
                },
                |position| {
                    (
                        1,
                        position.profile,
                        position.session_id,
                        position.model,
                        position.settings_hash,
                        position.day,
                        position.source_json,
                    )
                },
            );
            statement
                .query_map(
                    params![
                        account_id.get(),
                        local_machine.as_str(),
                        present,
                        profile,
                        session,
                        model,
                        settings,
                        day,
                        source,
                        limit
                    ],
                    raw_token,
                )
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
                .into_iter()
                .map(decode_token)
                .collect()
        })
    }

    fn advance_reporter_cursor(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        expected: ReporterCursorState,
        next: ReporterCursorState,
    ) -> StoreFuture<ReporterCursorState> {
        self.run(move |connection| {
            validate_reporter_destination(&destination_key)?;
            validate_reporter_transition(&expected, &next)?;
            let expected_cursor = expected.token_after.as_ref().map(encode).transpose()?;
            let next_cursor = next.token_after.as_ref().map(encode).transpose()?;
            let expected_generation = i64::try_from(expected.token_generation)
                .map_err(|_| PulseError::invalid_input("Pulse reporter generation is too large"))?;
            let next_generation = i64::try_from(next.token_generation)
                .map_err(|_| PulseError::invalid_input("Pulse reporter generation is too large"))?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let changed = transaction
                .execute(
                    "UPDATE reporter_cursors SET usage_after_id=?6,token_cursor_json=?7, \
                     token_generation=?8 WHERE account_id=?1 AND machine=?2 AND destination_key=?3 \
                     AND usage_after_id=?4 AND token_cursor_json IS ?5 AND token_generation=?9",
                    params![
                        account_id.get(),
                        local_machine.as_str(),
                        destination_key,
                        expected.usage_after_id,
                        expected_cursor,
                        next.usage_after_id,
                        next_cursor,
                        next_generation,
                        expected_generation
                    ],
                )
                .map_err(sql_error)?;
            if changed != 1 {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse reporter cursor changed concurrently",
                ));
            }
            transaction.commit().map_err(sql_error)?;
            Ok(next)
        })
    }

    fn load_reporter_pending(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        kind: ReporterStreamKind,
    ) -> StoreFuture<Option<ReporterPendingPage>> {
        self.run(move |connection| {
            validate_reporter_destination(&destination_key)?;
            load_sqlite_reporter_pending(
                connection,
                account_id,
                &local_machine,
                &destination_key,
                kind,
            )
        })
    }

    fn prepare_reporter_pending(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        draft: ReporterPendingDraft,
    ) -> StoreFuture<ReporterPendingPage> {
        self.run(move |connection| {
            validate_reporter_destination(&destination_key)?;
            draft.validate(account_id, &local_machine)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            if let Some(existing) = load_sqlite_reporter_pending(
                &transaction,
                account_id,
                &local_machine,
                &destination_key,
                draft.kind,
            )? {
                transaction.commit().map_err(sql_error)?;
                return Ok(existing);
            }
            let current = transaction
                .query_row(
                    "SELECT usage_after_id,token_cursor_json,token_generation \
                     FROM reporter_cursors WHERE account_id=?1 AND machine=?2 \
                     AND destination_key=?3",
                    params![account_id.get(), local_machine.as_str(), destination_key],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(sql_error)?
                .ok_or_else(|| {
                    PulseError::new(
                        PulseErrorKind::Conflict,
                        "Pulse reporter cursor was not initialized",
                    )
                })?;
            let current = ReporterCursorState {
                usage_after_id: current.0,
                token_after: current.1.as_deref().map(decode).transpose()?,
                token_generation: as_u64(current.2)?,
            };
            if current != draft.expected {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse reporter cursor changed before outbox preparation",
                ));
            }
            let pending = insert_sqlite_reporter_pending(
                &transaction,
                account_id,
                &local_machine,
                &destination_key,
                &draft,
            )?;
            transaction.commit().map_err(sql_error)?;
            Ok(pending)
        })
    }

    fn commit_reporter_pending(
        &self,
        account_id: AccountId,
        local_machine: MachineName,
        destination_key: String,
        kind: ReporterStreamKind,
        pending_id: i64,
    ) -> StoreFuture<ReporterCursorState> {
        self.run(move |connection| {
            validate_reporter_destination(&destination_key)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let pending = load_sqlite_reporter_pending(
                &transaction,
                account_id,
                &local_machine,
                &destination_key,
                kind,
            )?
            .ok_or_else(|| {
                PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse reporter outbox page is missing",
                )
            })?;
            if pending.id != pending_id {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse reporter outbox page changed concurrently",
                ));
            }
            let current = transaction
                .query_row(
                    "SELECT usage_after_id,token_cursor_json,token_generation \
                     FROM reporter_cursors WHERE account_id=?1 AND machine=?2 \
                     AND destination_key=?3",
                    params![account_id.get(), local_machine.as_str(), destination_key],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .map_err(sql_error)?;
            let current = ReporterCursorState {
                usage_after_id: current.0,
                token_after: current.1.as_deref().map(decode).transpose()?,
                token_generation: as_u64(current.2)?,
            };
            if current != pending.draft.expected {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse reporter cursor changed before outbox commit",
                ));
            }
            let next_cursor = pending
                .draft
                .next
                .token_after
                .as_ref()
                .map(encode)
                .transpose()?;
            let next_generation = i64::try_from(pending.draft.next.token_generation)
                .map_err(|_| PulseError::invalid_input("Pulse reporter generation is too large"))?;
            let changed = transaction
                .execute(
                    "UPDATE reporter_cursors SET usage_after_id=?4,token_cursor_json=?5, \
                     token_generation=?6 WHERE account_id=?1 AND machine=?2 AND destination_key=?3",
                    params![
                        account_id.get(),
                        local_machine.as_str(),
                        destination_key,
                        pending.draft.next.usage_after_id,
                        next_cursor,
                        next_generation
                    ],
                )
                .map_err(sql_error)?;
            if changed != 1 {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse reporter cursor changed before outbox commit",
                ));
            }
            let removed = transaction
                .execute(
                    "DELETE FROM reporter_pending_pages WHERE account_id=?1 AND id=?2",
                    params![account_id.get(), pending_id],
                )
                .map_err(sql_error)?;
            if removed != 1 {
                return Err(PulseError::new(
                    PulseErrorKind::Conflict,
                    "Pulse reporter outbox page changed concurrently",
                ));
            }
            transaction.commit().map_err(sql_error)?;
            Ok(pending.draft.next)
        })
    }

    fn ingest_batch(
        &self,
        account_id: AccountId,
        machine: MachineName,
        batch: IngestBatch,
        limits: IngestLimits,
    ) -> StoreFuture<IngestResult> {
        self.run(move |connection| {
            validate_ingest_scope(account_id, &machine, &batch, limits)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            enforce_ingest_caps(&transaction, account_id, &batch, limits)?;
            for profile in &batch.profiles {
                upsert_reported_profile(&transaction, profile)?;
            }
            for snapshot in &batch.snapshots {
                insert_snapshot(&transaction, snapshot)?;
            }
            upsert_token_batch(&transaction, &batch.token_grains)?;
            for session in &batch.context_sessions {
                upsert_context(&transaction, session)?;
            }
            for quota in &batch.gemini_quotas {
                upsert_gemini(&transaction, quota)?;
            }
            transaction.commit().map_err(sql_error)?;
            Ok(IngestResult {
                snapshots: batch.snapshots.len(),
                token_grains: batch.token_grains.len(),
                context_sessions: batch.context_sessions.len(),
                gemini_quotas: batch.gemini_quotas.len(),
            })
        })
    }

    fn ingest_batch_once(
        &self,
        account_id: AccountId,
        machine: MachineName,
        batch: IngestBatch,
        limits: IngestLimits,
        replay: IngestReplay,
    ) -> StoreFuture<IdempotentIngestResult> {
        self.run(move |connection| {
            ingest_batch_once(connection, account_id, &machine, &batch, limits, &replay)
        })
    }

    fn apply_retention(
        &self,
        now: Instant,
        context_days: u16,
        alert_days: u16,
        hourly_after_days: u16,
        daily_after_days: u16,
    ) -> StoreFuture<RetentionResult> {
        self.run(move |connection| {
            if context_days == 0
                || alert_days == 0
                || hourly_after_days == 0
                || daily_after_days <= hourly_after_days
            {
                return Err(PulseError::invalid_input(
                    "retention periods must be nonzero and daily must follow hourly",
                ));
            }
            let day_ms = 24_i64 * 60 * 60 * 1_000;
            let context_cutoff = now
                .epoch_millis()
                .saturating_sub(i64::from(context_days) * day_ms);
            let alert_cutoff = now
                .epoch_millis()
                .saturating_sub(i64::from(alert_days) * day_ms);
            let hourly_cutoff = now
                .epoch_millis()
                .saturating_sub(i64::from(hourly_after_days) * day_ms);
            let daily_cutoff = now
                .epoch_millis()
                .saturating_sub(i64::from(daily_after_days) * day_ms);
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let windows_before = row_count(&transaction, "usage_windows")?;
            let context_sessions = transaction
                .execute(
                    "DELETE FROM context_sessions WHERE last_active_at_ms < ?1",
                    params![context_cutoff],
                )
                .map_err(sql_error)?;
            let alert_events = transaction
                .execute(
                    "DELETE FROM alert_events WHERE triggered_at_ms < ?1",
                    params![alert_cutoff],
                )
                .map_err(sql_error)?;
            let daily_removed = transaction
                .execute(
                    "DELETE FROM usage_snapshots WHERE id IN (SELECT id FROM (\
                     SELECT id, ROW_NUMBER() OVER (PARTITION BY account_id, profile, machine, \
                     CAST(polled_at_ms / 86400000 AS INTEGER) ORDER BY polled_at_ms DESC, id DESC) AS rank \
                     FROM usage_snapshots WHERE polled_at_ms < ?1) WHERE rank > 1)",
                    params![daily_cutoff],
                )
                .map_err(sql_error)?;
            let hourly_removed = transaction
                .execute(
                    "DELETE FROM usage_snapshots WHERE id IN (SELECT id FROM (\
                     SELECT id, ROW_NUMBER() OVER (PARTITION BY account_id, profile, machine, \
                     CAST(polled_at_ms / 3600000 AS INTEGER) ORDER BY polled_at_ms DESC, id DESC) AS rank \
                     FROM usage_snapshots WHERE polled_at_ms >= ?1 AND polled_at_ms < ?2) WHERE rank > 1)",
                    params![daily_cutoff, hourly_cutoff],
                )
                .map_err(sql_error)?;
            let windows_after = row_count(&transaction, "usage_windows")?;
            transaction.commit().map_err(sql_error)?;
            Ok(RetentionResult {
                context_sessions,
                usage_windows: windows_before.saturating_sub(windows_after),
                usage_snapshots: daily_removed.saturating_add(hourly_removed),
                alert_events,
            })
        })
    }
}

fn upsert_profile(connection: &Connection, profile: &Profile) -> PulseResult<()> {
    profile.validate()?;
    if profile.origin == ProfileOrigin::Reported {
        return upsert_reported_profile(connection, profile);
    }
    connection
        .execute(
            "INSERT INTO profiles (account_id, name, vendor_json, config_dir, \
             poll_interval_minutes, monthly_budget_usd, api_key_env, api_key_file, \
             refresh_json, hidden, origin_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT(account_id, name) DO UPDATE SET vendor_json = excluded.vendor_json, \
             config_dir = excluded.config_dir, poll_interval_minutes = excluded.poll_interval_minutes, \
             monthly_budget_usd = excluded.monthly_budget_usd, api_key_env = excluded.api_key_env, \
             api_key_file = excluded.api_key_file, refresh_json = excluded.refresh_json, \
             hidden = excluded.hidden, origin_json = excluded.origin_json",
            params![
                profile.account_id.get(),
                profile.name.as_str(),
                encode(&profile.vendor)?,
                path_text(profile.config_dir.as_ref(), "config_dir")?,
                profile.poll_interval_minutes,
                profile.monthly_budget_usd,
                profile.api_key_env,
                path_text(profile.api_key_file.as_ref(), "api_key_file")?,
                encode(&profile.refresh)?,
                i64::from(profile.hidden),
                encode(&profile.origin)?
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn apply_sqlite_federated_row(connection: &Connection, row: &FederatedPulseRow) -> PulseResult<()> {
    match row {
        FederatedPulseRow::Machine(machine) => {
            if machine.last_seen < machine.first_seen {
                return Err(PulseError::invalid_input(
                    "machine last_seen cannot precede first_seen",
                ));
            }
            connection
                .execute(
                    "INSERT INTO machines (account_id, name, first_seen_ms, last_seen_ms) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT(account_id, name) DO UPDATE SET \
                     first_seen_ms=MIN(machines.first_seen_ms, excluded.first_seen_ms), \
                     last_seen_ms=MAX(machines.last_seen_ms, excluded.last_seen_ms)",
                    params![
                        machine.account_id.get(),
                        machine.name.as_str(),
                        machine.first_seen.epoch_millis(),
                        machine.last_seen.epoch_millis()
                    ],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        FederatedPulseRow::Profile(profile) => upsert_reported_profile(connection, profile),
        FederatedPulseRow::Usage(snapshot) => insert_snapshot(connection, snapshot).map(|_| ()),
        FederatedPulseRow::Context(session) => upsert_context(connection, session),
        FederatedPulseRow::Token(_) => Ok(()),
    }
}

fn upsert_reported_profile(connection: &Connection, profile: &Profile) -> PulseResult<()> {
    profile.validate()?;
    if profile.origin != ProfileOrigin::Reported {
        return Err(PulseError::invalid_input(
            "reported profile must have reported origin",
        ));
    }
    let existing = {
        let sql =
            format!("SELECT {PROFILE_COLUMNS} FROM profiles WHERE account_id = ?1 AND name = ?2");
        connection
            .query_row(
                &sql,
                params![profile.account_id.get(), profile.name.as_str()],
                raw_profile,
            )
            .optional()
            .map_err(sql_error)?
            .map(decode_profile)
            .transpose()?
    };
    if let Some(existing) = existing {
        if existing.vendor != profile.vendor {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "reported profile vendor conflicts with the stored profile",
            ));
        }
        if existing.origin == ProfileOrigin::Local {
            return Ok(());
        }
    }
    connection
        .execute(
            "INSERT INTO profiles (account_id, name, vendor_json, config_dir, \
             poll_interval_minutes, monthly_budget_usd, api_key_env, api_key_file, refresh_json, \
             hidden, origin_json) VALUES (?1,?2,?3,NULL,?4,?5,NULL,NULL,?6,?7,?8) \
             ON CONFLICT(account_id, name) DO UPDATE SET \
             vendor_json=excluded.vendor_json, poll_interval_minutes=excluded.poll_interval_minutes, \
             monthly_budget_usd=excluded.monthly_budget_usd, hidden=excluded.hidden, \
             origin_json=excluded.origin_json",
            params![
                profile.account_id.get(),
                profile.name.as_str(),
                encode(&profile.vendor)?,
                profile.poll_interval_minutes,
                profile.monthly_budget_usd,
                encode(&profile.refresh)?,
                i64::from(profile.hidden),
                encode(&profile.origin)?
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

struct RawSnapshot {
    id: i64,
    account_id: i64,
    profile: String,
    machine: String,
    vendor: String,
    outcome: String,
    polled_at: i64,
    reporter_version: Option<String>,
}

fn raw_snapshot(row: &Row<'_>) -> rusqlite::Result<RawSnapshot> {
    Ok(RawSnapshot {
        id: row.get(0)?,
        account_id: row.get(1)?,
        profile: row.get(2)?,
        machine: row.get(3)?,
        vendor: row.get(4)?,
        outcome: row.get(5)?,
        polled_at: row.get(6)?,
        reporter_version: row.get(7)?,
    })
}

fn decode_snapshot(connection: &Connection, raw: RawSnapshot) -> PulseResult<StoredUsageSnapshot> {
    let mut statement = connection
        .prepare(
            "SELECT kind_json, used_percent, resets_at_ms FROM usage_windows \
             WHERE snapshot_id = ?1 ORDER BY kind_json",
        )
        .map_err(sql_error)?;
    let windows = statement
        .query_map(params![raw.id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?
        .into_iter()
        .map(|(kind, used, resets)| {
            Ok(QuotaWindow {
                kind: decode(&kind)?,
                used_percent: Percent::new(used)?,
                resets_at: instant(resets)?,
            })
        })
        .collect::<PulseResult<Vec<_>>>()?;
    Ok(StoredUsageSnapshot {
        id: raw.id,
        snapshot: UsageSnapshot {
            account_id: AccountId::new(raw.account_id)?,
            profile: ProfileName::new(raw.profile)?,
            machine: MachineName::new(raw.machine)?,
            vendor: decode(&raw.vendor)?,
            windows,
            outcome: decode(&raw.outcome)?,
            polled_at: instant(raw.polled_at)?,
            reporter_version: raw.reporter_version,
        },
    })
}

struct RawCurrentWindow {
    machine: String,
    vendor: String,
    reporter_version: Option<String>,
    polled_at: i64,
    kind: String,
    used_percent: f64,
    resets_at: i64,
}

struct RawCurrentReport {
    kind: String,
    machine: String,
    reporter_version: Option<String>,
    polled_at: i64,
}

struct CurrentUsageState {
    windows: Vec<CurrentQuotaWindow>,
    keys: Vec<String>,
    winners: Vec<(MachineName, Instant)>,
}

fn load_current_usage(
    connection: &Connection,
    account_id: AccountId,
    profile: &ProfileName,
) -> PulseResult<Vec<CurrentQuotaWindow>> {
    let CurrentUsageState {
        mut windows,
        keys,
        winners,
    } = load_current_winners(connection, account_id, profile)?;
    for window in &mut windows {
        window.contributors.clear();
    }

    let mut seen = HashSet::new();
    for report in load_current_reports(connection, account_id, profile)? {
        let Some(index) = keys.iter().position(|key| key == &report.kind) else {
            continue;
        };
        if !seen.insert((report.kind, report.machine.clone())) {
            continue;
        }
        let machine = MachineName::new(report.machine)?;
        let polled_at = instant(report.polled_at)?;
        windows[index].contributors.push(UsageContributor {
            chosen: winners[index] == (machine.clone(), polled_at),
            machine,
            reporter_version: report.reporter_version,
            polled_at,
        });
    }
    for window in &mut windows {
        window.contributors.sort_by(|left, right| {
            right
                .chosen
                .cmp(&left.chosen)
                .then_with(|| left.machine.cmp(&right.machine))
        });
    }
    Ok(windows)
}

fn load_current_winners(
    connection: &Connection,
    account_id: AccountId,
    profile: &ProfileName,
) -> PulseResult<CurrentUsageState> {
    let mut statement = connection
        .prepare(
            "SELECT s.machine, s.vendor_json, s.reporter_version, s.polled_at_ms, \
             w.kind_json, w.used_percent, w.resets_at_ms \
             FROM usage_windows w JOIN usage_snapshots s ON s.id = w.snapshot_id \
             WHERE s.account_id = ?1 AND s.profile = ?2 AND w.accepted = 1 \
             ORDER BY w.kind_json, w.resets_at_ms DESC, s.polled_at_ms DESC, s.id DESC",
        )
        .map_err(sql_error)?;
    let candidates = statement
        .query_map(params![account_id.get(), profile.as_str()], |row| {
            Ok(RawCurrentWindow {
                machine: row.get(0)?,
                vendor: row.get(1)?,
                reporter_version: row.get(2)?,
                polled_at: row.get(3)?,
                kind: row.get(4)?,
                used_percent: row.get(5)?,
                resets_at: row.get(6)?,
            })
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;

    let mut windows = Vec::<CurrentQuotaWindow>::new();
    let mut keys = Vec::<String>::new();
    for candidate in candidates {
        let machine = MachineName::new(candidate.machine)?;
        if let Some(index) = keys.iter().position(|key| key == &candidate.kind) {
            if windows[index]
                .contributors
                .iter()
                .all(|item| item.machine != machine)
            {
                windows[index].contributors.push(UsageContributor {
                    machine,
                    reporter_version: candidate.reporter_version,
                    polled_at: instant(candidate.polled_at)?,
                    chosen: false,
                });
            }
            continue;
        }
        let polled_at = instant(candidate.polled_at)?;
        keys.push(candidate.kind.clone());
        windows.push(CurrentQuotaWindow {
            profile: profile.clone(),
            vendor: decode(&candidate.vendor)?,
            window: QuotaWindow {
                kind: decode(&candidate.kind)?,
                used_percent: Percent::new(candidate.used_percent)?,
                resets_at: instant(candidate.resets_at)?,
            },
            polled_at,
            contributors: vec![UsageContributor {
                machine,
                reporter_version: candidate.reporter_version,
                polled_at,
                chosen: true,
            }],
        });
    }
    let winners = windows
        .iter()
        .map(|window| {
            let winner = &window.contributors[0];
            (winner.machine.clone(), winner.polled_at)
        })
        .collect();
    Ok(CurrentUsageState {
        windows,
        keys,
        winners,
    })
}

fn load_current_reports(
    connection: &Connection,
    account_id: AccountId,
    profile: &ProfileName,
) -> PulseResult<Vec<RawCurrentReport>> {
    let mut statement = connection
        .prepare(
            "SELECT w.kind_json, s.machine, s.reporter_version, s.polled_at_ms \
             FROM usage_windows w JOIN usage_snapshots s ON s.id = w.snapshot_id \
             WHERE s.account_id = ?1 AND s.profile = ?2 \
             ORDER BY w.kind_json, s.machine, s.polled_at_ms DESC, s.id DESC",
        )
        .map_err(sql_error)?;
    statement
        .query_map(params![account_id.get(), profile.as_str()], |row| {
            Ok(RawCurrentReport {
                kind: row.get(0)?,
                machine: row.get(1)?,
                reporter_version: row.get(2)?,
                polled_at: row.get(3)?,
            })
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)
}

fn append_snapshot(connection: &mut Connection, snapshot: &UsageSnapshot) -> PulseResult<i64> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let id = insert_snapshot(&transaction, snapshot)?;
    transaction.commit().map_err(sql_error)?;
    Ok(id)
}

fn insert_snapshot(connection: &Connection, snapshot: &UsageSnapshot) -> PulseResult<i64> {
    snapshot.validate()?;
    connection
        .execute(
            "INSERT INTO usage_snapshots (account_id, profile, machine, vendor_json, \
             outcome_json, polled_at_ms, reporter_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                snapshot.account_id.get(),
                snapshot.profile.as_str(),
                snapshot.machine.as_str(),
                encode(&snapshot.vendor)?,
                encode(&snapshot.outcome)?,
                snapshot.polled_at.epoch_millis(),
                snapshot.reporter_version
            ],
        )
        .map_err(sql_error)?;
    let snapshot_id = connection.last_insert_rowid();
    for window in &snapshot.windows {
        let kind = encode(&window.kind)?;
        let previous = connection
            .query_row(
                "SELECT w.resets_at_ms, w.used_percent FROM usage_windows w \
                 JOIN usage_snapshots s ON s.id = w.snapshot_id \
                 WHERE s.account_id = ?1 AND s.profile = ?2 AND w.kind_json = ?3 AND w.accepted = 1 \
                 ORDER BY w.resets_at_ms DESC, s.polled_at_ms DESC, s.id DESC LIMIT 1",
                params![
                    snapshot.account_id.get(),
                    snapshot.profile.as_str(),
                    kind
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        let accepted = previous.is_none_or(|(old_reset, old_percent)| {
            window_is_accepted(snapshot.vendor, window, old_reset, old_percent)
        });
        connection
            .execute(
                "INSERT INTO usage_windows (snapshot_id, kind_json, used_percent, resets_at_ms, accepted) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    snapshot_id,
                    kind,
                    window.used_percent.get(),
                    window.resets_at.epoch_millis(),
                    i64::from(accepted)
                ],
            )
            .map_err(sql_error)?;
    }
    Ok(snapshot_id)
}

fn window_is_accepted(
    vendor: Vendor,
    window: &QuotaWindow,
    old_reset: i64,
    old_percent: f64,
) -> bool {
    let new_reset = window.resets_at.epoch_millis();
    if new_reset < old_reset.saturating_sub(RESET_JITTER_TOLERANCE_MS) {
        return false;
    }
    let same_period = new_reset.abs_diff(old_reset)
        <= u64::try_from(RESET_JITTER_TOLERANCE_MS).unwrap_or_default();
    !(same_period
        && vendor.rejects_same_period_decrease(window.kind)
        && window.used_percent.get() < old_percent)
}

struct RawContext {
    account_id: i64,
    profile: String,
    machine: String,
    session_id: String,
    model: Option<String>,
    settings: String,
    context_tokens: Option<i64>,
    context_percent: Option<f64>,
    effective_limit: Option<i64>,
    last_active_at: i64,
    last_reset_at: Option<i64>,
    collected_at: i64,
}

fn raw_context(row: &Row<'_>) -> rusqlite::Result<RawContext> {
    Ok(RawContext {
        account_id: row.get(0)?,
        profile: row.get(1)?,
        machine: row.get(2)?,
        session_id: row.get(3)?,
        model: row.get(4)?,
        settings: row.get(5)?,
        context_tokens: row.get(6)?,
        context_percent: row.get(7)?,
        effective_limit: row.get(8)?,
        last_active_at: row.get(9)?,
        last_reset_at: row.get(10)?,
        collected_at: row.get(11)?,
    })
}

fn decode_context(raw: RawContext) -> PulseResult<ContextSession> {
    Ok(ContextSession {
        account_id: AccountId::new(raw.account_id)?,
        profile: ProfileName::new(raw.profile)?,
        machine: MachineName::new(raw.machine)?,
        session_id: SessionId::new(raw.session_id)?,
        model: raw.model,
        settings: decode(&raw.settings)?,
        context_tokens: raw.context_tokens.map(as_u64).transpose()?,
        context_percent: raw.context_percent.map(Percent::new).transpose()?,
        effective_limit: raw.effective_limit.map(as_u64).transpose()?,
        last_active_at: instant(raw.last_active_at)?,
        last_reset_at: raw.last_reset_at.map(instant).transpose()?,
        collected_at: instant(raw.collected_at)?,
    })
}

fn upsert_context(connection: &Connection, session: &ContextSession) -> PulseResult<()> {
    session.validate()?;
    connection
        .execute(
            "INSERT INTO context_sessions (account_id, profile, machine, session_id, model, \
             settings_json, context_tokens, context_percent, effective_limit, last_active_at_ms, \
             last_reset_at_ms, collected_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
             ON CONFLICT(account_id, profile, machine, session_id) DO UPDATE SET \
             model = excluded.model, settings_json = excluded.settings_json, \
             context_tokens = excluded.context_tokens, context_percent = excluded.context_percent, \
             effective_limit = excluded.effective_limit, last_active_at_ms = excluded.last_active_at_ms, \
             last_reset_at_ms = excluded.last_reset_at_ms, collected_at_ms = excluded.collected_at_ms \
             WHERE excluded.collected_at_ms >= context_sessions.collected_at_ms",
            params![
                session.account_id.get(),
                session.profile.as_str(),
                session.machine.as_str(),
                session.session_id.as_str(),
                session.model,
                encode(&session.settings)?,
                session.context_tokens.map(|value| as_i64(value, "context_tokens")).transpose()?,
                session.context_percent.map(Percent::get),
                session.effective_limit.map(|value| as_i64(value, "effective_limit")).transpose()?,
                session.last_active_at.epoch_millis(),
                session.last_reset_at.map(Instant::epoch_millis),
                session.collected_at.epoch_millis()
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

struct RawToken {
    account_id: i64,
    profile: String,
    machine: String,
    session_id: String,
    model: String,
    settings_hash: String,
    settings: String,
    day: String,
    tokens_in: i64,
    tokens_out: i64,
    cache_write_5m: i64,
    cache_write_1h: i64,
    cache_read: i64,
    source: String,
}

fn raw_token(row: &Row<'_>) -> rusqlite::Result<RawToken> {
    Ok(RawToken {
        account_id: row.get(0)?,
        profile: row.get(1)?,
        machine: row.get(2)?,
        session_id: row.get(3)?,
        model: row.get(4)?,
        settings_hash: row.get(5)?,
        settings: row.get(6)?,
        day: row.get(7)?,
        tokens_in: row.get(8)?,
        tokens_out: row.get(9)?,
        cache_write_5m: row.get(10)?,
        cache_write_1h: row.get(11)?,
        cache_read: row.get(12)?,
        source: row.get(13)?,
    })
}

fn decode_token(raw: RawToken) -> PulseResult<TokenGrain> {
    Ok(TokenGrain {
        account_id: AccountId::new(raw.account_id)?,
        profile: ProfileName::new(raw.profile)?,
        machine: MachineName::new(raw.machine)?,
        session_id: SessionId::new(raw.session_id)?,
        model: raw.model,
        settings: decode(&raw.settings)?,
        settings_hash: raw.settings_hash,
        day: raw.day,
        tokens_in: as_u64(raw.tokens_in)?,
        tokens_out: as_u64(raw.tokens_out)?,
        cache_write_5m: as_u64(raw.cache_write_5m)?,
        cache_write_1h: as_u64(raw.cache_write_1h)?,
        cache_read: as_u64(raw.cache_read)?,
        source: decode(&raw.source)?,
    })
}

fn sqlite_token_write_revision(
    connection: &Connection,
    account_id: AccountId,
    profile: &ProfileName,
    machine: &MachineName,
) -> PulseResult<i64> {
    connection
        .execute(
            "INSERT INTO token_write_revisions(account_id,profile,machine,revision) \
             VALUES (?1,?2,?3,0) ON CONFLICT(account_id,profile,machine) DO NOTHING",
            params![account_id.get(), profile.as_str(), machine.as_str()],
        )
        .map_err(sql_error)?;
    connection
        .query_row(
            "SELECT revision FROM token_write_revisions \
             WHERE account_id=?1 AND profile=?2 AND machine=?3",
            params![account_id.get(), profile.as_str(), machine.as_str()],
            |row| row.get(0),
        )
        .map_err(sql_error)
}

fn validate_token_observation(
    observation: &TokenWriteObservation,
    grain: &TokenGrain,
) -> PulseResult<()> {
    observation.validate()?;
    grain.validate()?;
    if grain.account_id != observation.account_id
        || grain.profile != observation.profile
        || grain.machine != observation.machine
        || grain.source != TokenSource::Local
    {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "Pulse token observation is outside its reserved local scope",
        ));
    }
    Ok(())
}

fn allocate_sqlite_token_revision(
    connection: &Connection,
    account_id: AccountId,
    profile: &ProfileName,
    machine: &MachineName,
) -> PulseResult<i64> {
    let current = sqlite_token_write_revision(connection, account_id, profile, machine)?;
    let next = current.checked_add(1).ok_or_else(|| {
        PulseError::new(PulseErrorKind::Storage, "token write revision overflowed")
    })?;
    let updated = connection
        .execute(
            "UPDATE token_write_revisions SET revision=?4 \
             WHERE account_id=?1 AND profile=?2 AND machine=?3 AND revision=?5",
            params![
                account_id.get(),
                profile.as_str(),
                machine.as_str(),
                next,
                current
            ],
        )
        .map_err(sql_error)?;
    if updated != 1 {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "token write revision changed concurrently",
        ));
    }
    Ok(next)
}

fn token_matches(connection: &Connection, grain: &TokenGrain) -> PulseResult<bool> {
    let settings = encode(&grain.settings)?;
    let source = encode(&grain.source)?;
    connection
        .query_row(
            "SELECT settings_json=?9 AND tokens_in=?10 AND tokens_out=?11 \
                    AND cache_write_5m=?12 AND cache_write_1h=?13 AND cache_read=?14 \
             FROM token_usage WHERE account_id=?1 AND profile=?2 AND machine=?3 \
             AND session_id=?4 AND model=?5 AND settings_hash=?6 AND day=?7 AND source_json=?8",
            params![
                grain.account_id.get(),
                grain.profile.as_str(),
                grain.machine.as_str(),
                grain.session_id.as_str(),
                grain.model,
                grain.settings_hash,
                grain.day,
                source,
                settings,
                as_i64(grain.tokens_in, "tokens_in")?,
                as_i64(grain.tokens_out, "tokens_out")?,
                as_i64(grain.cache_write_5m, "cache_write_5m")?,
                as_i64(grain.cache_write_1h, "cache_write_1h")?,
                as_i64(grain.cache_read, "cache_read")?,
            ],
            |row| row.get(0),
        )
        .optional()
        .map(Option::unwrap_or_default)
        .map_err(sql_error)
}

fn upsert_token_batch<'a>(
    connection: &Connection,
    grains: impl IntoIterator<Item = &'a TokenGrain>,
) -> PulseResult<()> {
    let mut scopes = BTreeMap::<(AccountId, ProfileName, MachineName), Vec<&TokenGrain>>::new();
    for grain in grains {
        grain.validate()?;
        scopes
            .entry((
                grain.account_id,
                grain.profile.clone(),
                grain.machine.clone(),
            ))
            .or_default()
            .push(grain);
    }
    for ((account_id, profile, machine), rows) in scopes {
        let mut changed = Vec::new();
        for grain in rows {
            if !token_matches(connection, grain)? {
                changed.push(grain);
            }
        }
        if changed.is_empty() {
            continue;
        }
        let revision = allocate_sqlite_token_revision(connection, account_id, &profile, &machine)?;
        for grain in changed {
            upsert_token_at_revision(connection, grain, revision, false)?;
        }
    }
    Ok(())
}

fn upsert_token_at_revision(
    connection: &Connection,
    grain: &TokenGrain,
    revision: i64,
    conditional: bool,
) -> PulseResult<()> {
    grain.validate()?;
    let conditional = if conditional {
        " WHERE token_usage.write_revision < excluded.write_revision"
    } else {
        ""
    };
    let sql = format!(
        "INSERT INTO token_usage (account_id, profile, machine, session_id, model, \
         settings_hash, settings_json, day, tokens_in, tokens_out, cache_write_5m, \
         cache_write_1h, cache_read, source_json, updated_at_ms, write_revision) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16) \
         ON CONFLICT(account_id, profile, machine, session_id, model, settings_hash, day, source_json) \
         DO UPDATE SET settings_json = excluded.settings_json, tokens_in = excluded.tokens_in, \
         tokens_out = excluded.tokens_out, cache_write_5m = excluded.cache_write_5m, \
         cache_write_1h = excluded.cache_write_1h, cache_read = excluded.cache_read, \
         updated_at_ms = excluded.updated_at_ms, write_revision = excluded.write_revision{conditional}"
    );
    connection
        .execute(
            &sql,
            params![
                grain.account_id.get(),
                grain.profile.as_str(),
                grain.machine.as_str(),
                grain.session_id.as_str(),
                grain.model,
                grain.settings_hash,
                encode(&grain.settings)?,
                grain.day,
                as_i64(grain.tokens_in, "tokens_in")?,
                as_i64(grain.tokens_out, "tokens_out")?,
                as_i64(grain.cache_write_5m, "cache_write_5m")?,
                as_i64(grain.cache_write_1h, "cache_write_1h")?,
                as_i64(grain.cache_read, "cache_read")?,
                encode(&grain.source)?,
                Instant::now().epoch_millis(),
                revision
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn as_u64(value: i64) -> PulseResult<u64> {
    u64::try_from(value)
        .map_err(|_| PulseError::new(PulseErrorKind::Storage, "stored Pulse count is negative"))
}

struct RawPricing {
    key: String,
    vendor: String,
    model_pattern: String,
    settings: String,
    input: f64,
    output: f64,
    cache_5m: f64,
    cache_1h: f64,
    cache_read: f64,
}

fn raw_pricing(row: &Row<'_>) -> rusqlite::Result<RawPricing> {
    Ok(RawPricing {
        key: row.get(0)?,
        vendor: row.get(1)?,
        model_pattern: row.get(2)?,
        settings: row.get(3)?,
        input: row.get(4)?,
        output: row.get(5)?,
        cache_5m: row.get(6)?,
        cache_1h: row.get(7)?,
        cache_read: row.get(8)?,
    })
}

fn decode_pricing(raw: RawPricing) -> PulseResult<PricingRule> {
    Ok(PricingRule {
        key: raw.key,
        vendor: decode(&raw.vendor)?,
        model_pattern: raw.model_pattern,
        settings_match: decode(&raw.settings)?,
        input_per_million_usd: raw.input,
        output_per_million_usd: raw.output,
        cache_write_5m_per_million_usd: raw.cache_5m,
        cache_write_1h_per_million_usd: raw.cache_1h,
        cache_read_per_million_usd: raw.cache_read,
    })
}

fn upsert_pricing_default(connection: &Connection, rule: &PricingRule) -> PulseResult<()> {
    rule.validate()?;
    let vendor = encode(&rule.vendor)?;
    let settings = encode(&rule.settings_match)?;
    connection
        .execute(
            "INSERT INTO pricing_defaults (key, vendor_json, model_pattern, settings_json, \
             input_rate, output_rate, cache_write_5m_rate, cache_write_1h_rate, cache_read_rate) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT(key) DO UPDATE SET \
             vendor_json = excluded.vendor_json, model_pattern = excluded.model_pattern, \
             settings_json = excluded.settings_json, input_rate = excluded.input_rate, \
             output_rate = excluded.output_rate, cache_write_5m_rate = excluded.cache_write_5m_rate, \
             cache_write_1h_rate = excluded.cache_write_1h_rate, cache_read_rate = excluded.cache_read_rate",
            params![
                rule.key,
                vendor,
                rule.model_pattern,
                settings,
                rule.input_per_million_usd,
                rule.output_per_million_usd,
                rule.cache_write_5m_per_million_usd,
                rule.cache_write_1h_per_million_usd,
                rule.cache_read_per_million_usd
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn upsert_pricing_override(
    connection: &Connection,
    account_id: AccountId,
    rule: &PricingRule,
) -> PulseResult<()> {
    rule.validate()?;
    connection
        .execute(
            "INSERT INTO pricing_overrides (account_id, key, vendor_json, model_pattern, \
             settings_json, input_rate, output_rate, cache_write_5m_rate, cache_write_1h_rate, \
             cache_read_rate) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(account_id, key) DO UPDATE SET vendor_json = excluded.vendor_json, \
             model_pattern = excluded.model_pattern, settings_json = excluded.settings_json, \
             input_rate = excluded.input_rate, output_rate = excluded.output_rate, \
             cache_write_5m_rate = excluded.cache_write_5m_rate, \
             cache_write_1h_rate = excluded.cache_write_1h_rate, cache_read_rate = excluded.cache_read_rate",
            params![
                account_id.get(),
                rule.key,
                encode(&rule.vendor)?,
                rule.model_pattern,
                encode(&rule.settings_match)?,
                rule.input_per_million_usd,
                rule.output_per_million_usd,
                rule.cache_write_5m_per_million_usd,
                rule.cache_write_1h_per_million_usd,
                rule.cache_read_per_million_usd
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn alert_threshold_key(threshold: Option<Percent>) -> String {
    threshold.map_or_else(|| "none".to_owned(), |value| format!("{:.9}", value.get()))
}

fn upsert_import_alert_subscription(
    connection: &Connection,
    stored: &ImportedAlertSubscription,
) -> PulseResult<()> {
    let subscription = &stored.subscription;
    subscription.validate()?;
    let alert_type = encode(&subscription.alert_type)?;
    let delivery = subscription.delivery.as_ref().map(encode).transpose()?;
    let threshold = subscription.threshold.map(Percent::get);
    let threshold_key = alert_threshold_key(subscription.threshold);
    connection
        .execute(
            "INSERT INTO alert_subscriptions (account_id, profile, alert_type_json, threshold, \
             threshold_key, cooldown_minutes, delivery_json, enabled, created_at_ms) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
             ON CONFLICT(account_id, profile, alert_type_json, threshold_key) DO UPDATE SET \
             cooldown_minutes=excluded.cooldown_minutes, delivery_json=excluded.delivery_json, \
             enabled=excluded.enabled",
            params![
                subscription.account_id.get(),
                subscription.profile.as_str(),
                alert_type,
                threshold,
                threshold_key,
                subscription.cooldown_minutes,
                delivery,
                i64::from(subscription.enabled),
                stored.created_at.epoch_millis()
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn insert_import_alert_event(
    connection: &Connection,
    imported: &ImportedAlertEvent,
) -> PulseResult<()> {
    let subscription = &imported.subscription;
    let input = &imported.input;
    subscription.validate()?;
    if input.account_id != subscription.account_id
        || input.profile != subscription.profile
        || input.alert_type != subscription.alert_type
        || input.message.is_empty()
        || input.message.len() > 4_096
    {
        return Err(PulseError::invalid_input(
            "imported alert event does not match its bounded subscription",
        ));
    }
    let alert_type = encode(&subscription.alert_type)?;
    let threshold_key = alert_threshold_key(subscription.threshold);
    let subscription_id = connection
        .query_row(
            "SELECT id FROM alert_subscriptions WHERE account_id=?1 AND profile=?2 \
             AND alert_type_json=?3 AND threshold_key=?4",
            params![
                subscription.account_id.get(),
                subscription.profile.as_str(),
                alert_type,
                threshold_key
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(sql_error)?;
    connection
        .execute(
            "INSERT INTO alert_events (account_id, subscription_id, profile, alert_type_json, \
             message, current_value, threshold, acknowledged, triggered_at_ms) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                input.account_id.get(),
                subscription_id,
                input.profile.as_str(),
                alert_type,
                input.message,
                input.current_value.map(Percent::get),
                input.threshold.map(Percent::get),
                i64::from(imported.acknowledged),
                input.triggered_at.epoch_millis()
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

struct RawAlertSubscription {
    id: i64,
    account_id: i64,
    profile: String,
    alert_type: String,
    threshold: Option<f64>,
    cooldown_minutes: u32,
    delivery: Option<String>,
    enabled: bool,
    created_at: i64,
}

fn raw_alert_subscription(row: &Row<'_>) -> rusqlite::Result<RawAlertSubscription> {
    Ok(RawAlertSubscription {
        id: row.get(0)?,
        account_id: row.get(1)?,
        profile: row.get(2)?,
        alert_type: row.get(3)?,
        threshold: row.get(4)?,
        cooldown_minutes: row.get(5)?,
        delivery: row.get(6)?,
        enabled: row.get::<_, i64>(7)? != 0,
        created_at: row.get(8)?,
    })
}

fn decode_alert_subscription(raw: RawAlertSubscription) -> PulseResult<StoredAlertSubscription> {
    Ok(StoredAlertSubscription {
        id: raw.id,
        subscription: AlertSubscription {
            account_id: AccountId::new(raw.account_id)?,
            profile: ProfileName::new(raw.profile)?,
            alert_type: decode(&raw.alert_type)?,
            threshold: raw.threshold.map(Percent::new).transpose()?,
            cooldown_minutes: raw.cooldown_minutes,
            delivery: raw.delivery.as_deref().map(decode).transpose()?,
            enabled: raw.enabled,
        },
        created_at: instant(raw.created_at)?,
    })
}

struct RawAlertEvent {
    id: i64,
    account_id: i64,
    subscription_id: i64,
    profile: String,
    alert_type: String,
    message: String,
    current_value: Option<f64>,
    threshold: Option<f64>,
    acknowledged: bool,
    triggered_at: i64,
}

fn raw_alert_event(row: &Row<'_>) -> rusqlite::Result<RawAlertEvent> {
    Ok(RawAlertEvent {
        id: row.get(0)?,
        account_id: row.get(1)?,
        subscription_id: row.get(2)?,
        profile: row.get(3)?,
        alert_type: row.get(4)?,
        message: row.get(5)?,
        current_value: row.get(6)?,
        threshold: row.get(7)?,
        acknowledged: row.get::<_, i64>(8)? != 0,
        triggered_at: row.get(9)?,
    })
}

fn decode_alert_event(raw: RawAlertEvent) -> PulseResult<AlertEvent> {
    Ok(AlertEvent {
        id: raw.id,
        input: AlertEventInput {
            account_id: AccountId::new(raw.account_id)?,
            subscription_id: raw.subscription_id,
            profile: ProfileName::new(raw.profile)?,
            alert_type: decode(&raw.alert_type)?,
            message: raw.message,
            current_value: raw.current_value.map(Percent::new).transpose()?,
            threshold: raw.threshold.map(Percent::new).transpose()?,
            triggered_at: instant(raw.triggered_at)?,
        },
        acknowledged: raw.acknowledged,
    })
}

fn raw_alert_reply(row: &Row<'_>) -> rusqlite::Result<(i64, i64, i64, String, i64)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn decode_alert_reply(raw: (i64, i64, i64, String, i64)) -> PulseResult<AlertReply> {
    Ok(AlertReply {
        id: raw.0,
        account_id: AccountId::new(raw.1)?,
        event_id: raw.2,
        message: raw.3,
        replied_at: instant(raw.4)?,
    })
}

type RawResetResume = (
    i64,
    i64,
    String,
    i64,
    i64,
    i64,
    Option<i64>,
    i64,
    Option<i64>,
    Option<i64>,
);

fn raw_reset_resume(row: &Row<'_>) -> rusqlite::Result<RawResetResume> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn decode_reset_resume(raw: RawResetResume) -> PulseResult<ResetResumeJob> {
    Ok(ResetResumeJob {
        id: raw.0,
        input: ResetResumeInput {
            account_id: AccountId::new(raw.1)?,
            profile: ProfileName::new(raw.2)?,
            resets_at: instant(raw.3)?,
            scheduled_at: instant(raw.5)?,
        },
        resume_at: instant(raw.4)?,
        lease_until: raw.6.map(instant).transpose()?,
        attempts: u32::try_from(raw.7).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Storage,
                "stored reset attempt count is invalid",
            )
        })?,
        delivered_at: raw.8.map(instant).transpose()?,
        cancelled_at: raw.9.map(instant).transpose()?,
    })
}

fn validate_reply(reply: &AlertReplyInput) -> PulseResult<()> {
    if reply.event_id <= 0
        || reply.message.is_empty()
        || reply.message.len() > MAX_ALERT_REPLY_BYTES
        || reply
            .message
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(PulseError::invalid_input(
            "alert reply must be nonempty, bounded safe text",
        ));
    }
    Ok(())
}

fn reply_to_alert(
    connection: &mut Connection,
    reply: &AlertReplyInput,
) -> PulseResult<Option<AlertReply>> {
    validate_reply(reply)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let exists = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM alert_events WHERE account_id=?1 AND id=?2)",
            params![reply.account_id.get(), reply.event_id],
            |row| row.get::<_, bool>(0),
        )
        .map_err(sql_error)?;
    if !exists {
        return Ok(None);
    }
    transaction
        .execute(
            "UPDATE alert_events SET acknowledged=1 WHERE account_id=?1 AND id=?2",
            params![reply.account_id.get(), reply.event_id],
        )
        .map_err(sql_error)?;
    let reply_count = transaction
        .query_row(
            "SELECT COUNT(*) FROM alert_replies WHERE account_id=?1 AND event_id=?2",
            params![reply.account_id.get(), reply.event_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(sql_error)
        .and_then(stored_count)?;
    if reply_count >= MAX_ALERT_REPLIES_PER_EVENT {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "alert reply count reached its event cap",
        ));
    }
    transaction
        .execute(
            "INSERT INTO alert_replies (account_id,event_id,message,replied_at_ms) \
             VALUES (?1,?2,?3,?4)",
            params![
                reply.account_id.get(),
                reply.event_id,
                reply.message,
                reply.replied_at.epoch_millis()
            ],
        )
        .map_err(sql_error)?;
    let stored = AlertReply {
        id: transaction.last_insert_rowid(),
        account_id: reply.account_id,
        event_id: reply.event_id,
        message: reply.message.clone(),
        replied_at: reply.replied_at,
    };
    transaction.commit().map_err(sql_error)?;
    Ok(Some(stored))
}

fn validate_reset_input(input: &ResetResumeInput, limits: ResetResumeLimits) -> PulseResult<i64> {
    if limits.max_pending_per_account == 0
        || limits.max_pending_per_account > MAX_RESET_JOBS_PER_ACCOUNT
        || limits.max_horizon_millis == 0
        || limits.max_horizon_millis > MAX_RESET_HORIZON_MILLIS
    {
        return Err(PulseError::invalid_input(
            "reset resume limits are out of bounds",
        ));
    }
    let reset_delta = input
        .resets_at
        .epoch_millis()
        .checked_sub(input.scheduled_at.epoch_millis())
        .filter(|delta| *delta > 0)
        .ok_or_else(|| PulseError::invalid_input("reset must be in the future"))?;
    let max_horizon = i64::try_from(limits.max_horizon_millis)
        .map_err(|_| PulseError::invalid_input("reset horizon is too large"))?;
    if reset_delta > max_horizon {
        return Err(PulseError::invalid_input(
            "reset exceeds the scheduling horizon",
        ));
    }
    input
        .resets_at
        .epoch_millis()
        .checked_add(60_000)
        .ok_or_else(|| PulseError::invalid_input("reset resume time overflowed"))
}

fn schedule_reset_resume(
    connection: &mut Connection,
    input: &ResetResumeInput,
    limits: ResetResumeLimits,
) -> PulseResult<ResetResumeJob> {
    let resume_at = validate_reset_input(input, limits)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let existing = transaction
        .query_row(
            "SELECT id, account_id, profile, resets_at_ms, resume_at_ms, scheduled_at_ms, \
             lease_until_ms, attempts, delivered_at_ms, cancelled_at_ms FROM reset_resume_jobs \
             WHERE account_id=?1 AND profile=?2 AND resets_at_ms=?3",
            params![
                input.account_id.get(),
                input.profile.as_str(),
                input.resets_at.epoch_millis()
            ],
            raw_reset_resume,
        )
        .optional()
        .map_err(sql_error)?
        .map(decode_reset_resume)
        .transpose()?;
    if let Some(existing) = existing
        && (existing.delivered_at.is_some() || existing.cancelled_at.is_none())
    {
        transaction.commit().map_err(sql_error)?;
        return Ok(existing);
    }
    let pending = transaction
        .query_row(
            "SELECT COUNT(*) FROM reset_resume_jobs WHERE account_id=?1 \
             AND delivered_at_ms IS NULL AND cancelled_at_ms IS NULL",
            params![input.account_id.get()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(sql_error)
        .and_then(stored_count)?;
    if pending >= limits.max_pending_per_account {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "reset resume jobs reached the account cap",
        ));
    }
    transaction
        .execute(
            "INSERT INTO reset_resume_jobs (account_id,profile,resets_at_ms,resume_at_ms, \
             scheduled_at_ms,lease_until_ms,attempts,delivered_at_ms,cancelled_at_ms) \
             VALUES (?1,?2,?3,?4,?5,NULL,0,NULL,NULL) \
             ON CONFLICT(account_id,profile,resets_at_ms) DO UPDATE SET \
             resume_at_ms=excluded.resume_at_ms, scheduled_at_ms=excluded.scheduled_at_ms, \
             lease_until_ms=NULL, attempts=0, delivered_at_ms=NULL, cancelled_at_ms=NULL",
            params![
                input.account_id.get(),
                input.profile.as_str(),
                input.resets_at.epoch_millis(),
                resume_at,
                input.scheduled_at.epoch_millis()
            ],
        )
        .map_err(sql_error)?;
    let raw = transaction
        .query_row(
            "SELECT id, account_id, profile, resets_at_ms, resume_at_ms, scheduled_at_ms, \
             lease_until_ms, attempts, delivered_at_ms, cancelled_at_ms FROM reset_resume_jobs \
             WHERE account_id=?1 AND profile=?2 AND resets_at_ms=?3",
            params![
                input.account_id.get(),
                input.profile.as_str(),
                input.resets_at.epoch_millis()
            ],
            raw_reset_resume,
        )
        .map_err(sql_error)?;
    let job = decode_reset_resume(raw)?;
    transaction.commit().map_err(sql_error)?;
    Ok(job)
}

fn claim_due_reset_resumes(
    connection: &mut Connection,
    account_id: AccountId,
    now: Instant,
    lease_until: Instant,
    limit: usize,
) -> PulseResult<Vec<ResetResumeJob>> {
    if lease_until <= now {
        return Err(PulseError::invalid_input("reset lease must end after now"));
    }
    let limit = query_limit(limit)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let ids = {
        let mut statement = transaction
            .prepare(
                "SELECT id FROM reset_resume_jobs WHERE account_id=?1 AND resume_at_ms<=?2 \
                 AND delivered_at_ms IS NULL AND cancelled_at_ms IS NULL \
                 AND (lease_until_ms IS NULL OR lease_until_ms<=?2) \
                 ORDER BY resume_at_ms,id LIMIT ?3",
            )
            .map_err(sql_error)?;
        statement
            .query_map(
                params![account_id.get(), now.epoch_millis(), limit],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?
    };
    let mut jobs = Vec::with_capacity(ids.len());
    for id in ids {
        transaction
            .execute(
                "UPDATE reset_resume_jobs SET lease_until_ms=?3, attempts=attempts+1 \
                 WHERE account_id=?1 AND id=?2",
                params![account_id.get(), id, lease_until.epoch_millis()],
            )
            .map_err(sql_error)?;
        let raw = transaction
            .query_row(
                "SELECT id, account_id, profile, resets_at_ms, resume_at_ms, scheduled_at_ms, \
                 lease_until_ms, attempts, delivered_at_ms, cancelled_at_ms \
                 FROM reset_resume_jobs WHERE account_id=?1 AND id=?2",
                params![account_id.get(), id],
                raw_reset_resume,
            )
            .map_err(sql_error)?;
        jobs.push(decode_reset_resume(raw)?);
    }
    transaction.commit().map_err(sql_error)?;
    Ok(jobs)
}

fn record_alert_if_due(
    connection: &mut Connection,
    event: &AlertEventInput,
) -> PulseResult<Option<AlertEvent>> {
    if event.message.is_empty() || event.message.len() > 4_096 {
        return Err(PulseError::invalid_input(
            "alert message must be between 1 and 4096 bytes",
        ));
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let subscription = transaction
        .query_row(
            "SELECT id, account_id, profile, alert_type_json, threshold, cooldown_minutes, \
             delivery_json, enabled, created_at_ms FROM alert_subscriptions \
             WHERE account_id = ?1 AND id = ?2",
            params![event.account_id.get(), event.subscription_id],
            raw_alert_subscription,
        )
        .optional()
        .map_err(sql_error)?
        .ok_or_else(|| PulseError::new(PulseErrorKind::NotFound, "alert subscription not found"))?;
    let subscription = decode_alert_subscription(subscription)?;
    if !subscription.subscription.enabled {
        return Ok(None);
    }
    if subscription.subscription.profile != event.profile
        || subscription.subscription.alert_type != event.alert_type
    {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "alert event does not match its account-scoped subscription",
        ));
    }
    if subscription.subscription.threshold != event.threshold {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "alert event threshold does not match its account-scoped subscription",
        ));
    }
    match event.alert_type {
        AlertType::FiveHourThreshold
        | AlertType::SevenDayThreshold
        | AlertType::ContextThreshold => {
            let (Some(current), Some(threshold)) = (event.current_value, event.threshold) else {
                return Err(PulseError::invalid_input(
                    "threshold alerts require threshold and current value",
                ));
            };
            if current.get() < threshold.get() {
                return Err(PulseError::invalid_input(
                    "threshold alert current value is below its stored threshold",
                ));
            }
        }
        AlertType::AuthenticationFailure => {
            if event.threshold.is_some() || event.current_value.is_some() {
                return Err(PulseError::invalid_input(
                    "authentication alerts cannot contain threshold values",
                ));
            }
        }
    }
    let last = transaction
        .query_row(
            "SELECT triggered_at_ms FROM alert_events WHERE account_id = ?1 AND subscription_id = ?2 \
             ORDER BY triggered_at_ms DESC, id DESC LIMIT 1",
            params![event.account_id.get(), event.subscription_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(sql_error)?;
    let cooldown_ms = i64::from(subscription.subscription.cooldown_minutes) * 60 * 1_000;
    if last.is_some_and(|last| event.triggered_at.epoch_millis() < last.saturating_add(cooldown_ms))
    {
        return Ok(None);
    }
    transaction
        .execute(
            "INSERT INTO alert_events (account_id, subscription_id, profile, alert_type_json, \
             message, current_value, threshold, acknowledged, triggered_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)",
            params![
                event.account_id.get(),
                event.subscription_id,
                event.profile.as_str(),
                encode(&event.alert_type)?,
                event.message,
                event.current_value.map(Percent::get),
                event.threshold.map(Percent::get),
                event.triggered_at.epoch_millis()
            ],
        )
        .map_err(sql_error)?;
    let id = transaction.last_insert_rowid();
    transaction.commit().map_err(sql_error)?;
    Ok(Some(AlertEvent {
        id,
        input: event.clone(),
        acknowledged: false,
    }))
}

fn upsert_gemini(connection: &Connection, quota: &GeminiQuota) -> PulseResult<()> {
    quota.validate()?;
    connection
        .execute(
            "INSERT INTO gemini_quota (account_id, model_id, remaining_fraction, remaining_amount, \
             resets_at_ms, collected_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(account_id, model_id) DO UPDATE SET \
             remaining_fraction = excluded.remaining_fraction, \
             remaining_amount = excluded.remaining_amount, resets_at_ms = excluded.resets_at_ms, \
             collected_at_ms = excluded.collected_at_ms \
             WHERE excluded.collected_at_ms >= gemini_quota.collected_at_ms",
            params![
                quota.account_id.get(),
                quota.model_id,
                quota.remaining_fraction.get(),
                quota.remaining_amount,
                quota.resets_at.map(Instant::epoch_millis),
                quota.collected_at.epoch_millis()
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn validate_token_hash(value: &str) -> PulseResult<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(PulseError::invalid_input(
            "ingest token hash must be lowercase SHA-256 hex",
        ));
    }
    Ok(())
}

fn load_sqlite_token_backfill(
    connection: &Connection,
    account_id: AccountId,
    profile: &ProfileName,
    machine: &MachineName,
) -> PulseResult<Option<TokenBackfillState>> {
    let raw = connection
        .query_row(
            "SELECT generation,source_generation,write_revision,cursor_json,complete \
             FROM backfill_progress \
             WHERE account_id=?1 AND profile=?2 AND machine=?3",
            params![account_id.get(), profile.as_str(), machine.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    raw.map(|(generation, source, write_revision, cursor, complete)| {
        let state = TokenBackfillState {
            account_id,
            profile: profile.clone(),
            machine: machine.clone(),
            generation: as_u64(generation)?,
            source_generation: TokenSourceGeneration::new(source)?,
            write_revision: as_u64(write_revision)?,
            cursor: cursor.as_deref().map(decode).transpose()?,
            complete: match complete {
                0 => false,
                1 => true,
                _ => {
                    return Err(PulseError::new(
                        PulseErrorKind::Storage,
                        "stored backfill completion flag is invalid",
                    ));
                }
            },
        };
        state.validate()?;
        Ok(state)
    })
    .transpose()
}

fn begin_sqlite_token_backfill(
    connection: &mut Connection,
    account_id: AccountId,
    profile: &ProfileName,
    machine: &MachineName,
    source_generation: &TokenSourceGeneration,
    restart_completed: bool,
) -> PulseResult<TokenBackfillState> {
    source_generation.validate()?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let current = load_sqlite_token_backfill(&transaction, account_id, profile, machine)?;
    let state = match current {
        None => {
            let write_revision =
                allocate_sqlite_token_revision(&transaction, account_id, profile, machine)?;
            transaction
                .execute(
                    "INSERT INTO backfill_progress \
                     (account_id,profile,machine,generation,source_generation,cursor_json, \
                      write_revision,complete,updated_at_ms) VALUES (?1,?2,?3,1,?4,NULL,?5,0, \
                      CAST(unixepoch('subsec') * 1000 AS INTEGER))",
                    params![
                        account_id.get(),
                        profile.as_str(),
                        machine.as_str(),
                        source_generation.as_str(),
                        write_revision
                    ],
                )
                .map_err(sql_error)?;
            load_sqlite_token_backfill(&transaction, account_id, profile, machine)?
                .ok_or_else(|| PulseError::new(PulseErrorKind::Storage, "backfill insert failed"))?
        }
        Some(mut state)
            if state.source_generation != *source_generation
                || (state.complete && restart_completed) =>
        {
            let write_revision =
                allocate_sqlite_token_revision(&transaction, account_id, profile, machine)?;
            state.generation = state.generation.checked_add(1).ok_or_else(|| {
                PulseError::new(PulseErrorKind::Storage, "backfill generation overflowed")
            })?;
            state.source_generation = source_generation.clone();
            state.write_revision = as_u64(write_revision)?;
            state.cursor = None;
            state.complete = false;
            transaction
                .execute(
                    "UPDATE backfill_progress SET generation=?4,source_generation=?5, \
                     write_revision=?6,cursor_json=NULL,complete=0,updated_at_ms= \
                     CAST(unixepoch('subsec') * 1000 AS INTEGER) \
                     WHERE account_id=?1 AND profile=?2 AND machine=?3",
                    params![
                        account_id.get(),
                        profile.as_str(),
                        machine.as_str(),
                        i64::try_from(state.generation).map_err(|_| PulseError::new(
                            PulseErrorKind::Storage,
                            "backfill generation overflowed"
                        ))?,
                        source_generation.as_str(),
                        write_revision
                    ],
                )
                .map_err(sql_error)?;
            state
        }
        Some(state) => state,
    };
    transaction.commit().map_err(sql_error)?;
    Ok(state)
}

fn apply_sqlite_token_backfill_page(
    connection: &mut Connection,
    page: &TokenBackfillPage,
) -> PulseResult<TokenBackfillState> {
    page.validate()?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let current = load_sqlite_token_backfill(
        &transaction,
        page.expected.account_id,
        &page.expected.profile,
        &page.expected.machine,
    )?
    .ok_or_else(|| PulseError::new(PulseErrorKind::Conflict, "backfill cursor is missing"))?;
    if current != page.expected {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "backfill cursor changed concurrently",
        ));
    }
    let revision = i64::try_from(page.expected.write_revision).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "backfill write revision overflowed",
        )
    })?;
    for row in &page.rows {
        upsert_token_at_revision(&transaction, row, revision, true)?;
    }
    let cursor = page.next_cursor.as_ref().map(encode).transpose()?;
    transaction
        .execute(
            "UPDATE backfill_progress SET cursor_json=?4,complete=?5,updated_at_ms= \
             CAST(unixepoch('subsec') * 1000 AS INTEGER) \
             WHERE account_id=?1 AND profile=?2 AND machine=?3",
            params![
                page.expected.account_id.get(),
                page.expected.profile.as_str(),
                page.expected.machine.as_str(),
                cursor,
                i64::from(page.complete)
            ],
        )
        .map_err(sql_error)?;
    let mut next = page.expected.clone();
    next.cursor.clone_from(&page.next_cursor);
    next.complete = page.complete;
    transaction.commit().map_err(sql_error)?;
    Ok(next)
}

fn validate_import(provenance: &ImportProvenance) -> PulseResult<()> {
    validate_token_hash(&provenance.source_fingerprint)?;
    validate_token_hash(&provenance.payload_fingerprint)?;
    for (name, value, limit) in [
        ("source_table", provenance.source_table.as_str(), 128),
        ("source_row_id", provenance.source_row_id.as_str(), 256),
        ("target_key", provenance.target_key.as_str(), 1_024),
    ] {
        if value.is_empty() || value.len() > limit || value.chars().any(char::is_control) {
            return Err(PulseError::invalid_input(format!(
                "import {name} must be nonempty, bounded text"
            )));
        }
    }
    Ok(())
}

fn validate_reconciliation_keys(keys: &[TokenReconciliationKey]) -> PulseResult<()> {
    if keys.is_empty() || keys.len() > MAX_IMPORT_RECONCILIATION_KEYS {
        return Err(PulseError::invalid_input(
            "Pulse token reconciliation key count is outside its bounds",
        ));
    }
    let mut unique = HashSet::with_capacity(keys.len());
    for key in keys {
        jiff::civil::Date::from_str(&key.day)
            .map_err(|_| PulseError::invalid_input("Pulse token reconciliation day is invalid"))?;
        if !unique.insert((key.profile.clone(), key.day.clone())) {
            return Err(PulseError::invalid_input(
                "Pulse token reconciliation contains duplicate keys",
            ));
        }
    }
    Ok(())
}

fn nonnegative_total(value: i64) -> PulseResult<u128> {
    u128::try_from(value).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "stored Pulse token aggregate is invalid",
        )
    })
}

fn validate_import_batch(batch: &ImportBatch) -> PulseResult<()> {
    if batch.row_count() == 0 || batch.row_count() > MAX_IMPORT_BATCH_ROWS {
        return Err(PulseError::invalid_input(
            "Pulse import batch exceeds its bounded row limit",
        ));
    }
    if batch
        .prerequisite_machines
        .iter()
        .any(|row| row.account_id != batch.account_id)
        || batch
            .profiles
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .machines
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .snapshots
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .token_grains
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .context_sessions
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .gemini_quotas
            .iter()
            .any(|row| !import_row_matches(row, batch.account_id, |value| value.account_id))
        || batch
            .pricing_overrides
            .iter()
            .any(|row| row.provenance.account_id != batch.account_id)
        || batch.alert_subscriptions.iter().any(|row| {
            !import_row_matches(row, batch.account_id, |value| value.subscription.account_id)
        })
        || batch.alert_events.iter().any(|row| {
            !import_row_matches(row, batch.account_id, |value| value.input.account_id)
                || row.value.subscription.account_id != batch.account_id
        })
    {
        return Err(PulseError::invalid_input(
            "Pulse import batch contains a cross-account row",
        ));
    }
    for provenance in batch
        .profiles
        .iter()
        .map(|row| &row.provenance)
        .chain(batch.machines.iter().map(|row| &row.provenance))
        .chain(batch.snapshots.iter().map(|row| &row.provenance))
        .chain(batch.token_grains.iter().map(|row| &row.provenance))
        .chain(batch.context_sessions.iter().map(|row| &row.provenance))
        .chain(batch.gemini_quotas.iter().map(|row| &row.provenance))
        .chain(batch.pricing_overrides.iter().map(|row| &row.provenance))
        .chain(batch.alert_subscriptions.iter().map(|row| &row.provenance))
        .chain(batch.alert_events.iter().map(|row| &row.provenance))
    {
        validate_import(provenance)?;
    }
    Ok(())
}

fn import_row_matches<T>(
    row: &ImportedRow<T>,
    account_id: AccountId,
    value_account: impl FnOnce(&T) -> AccountId,
) -> bool {
    row.provenance.account_id == account_id && value_account(&row.value) == account_id
}

fn claim_import(connection: &Connection, provenance: &ImportProvenance) -> PulseResult<bool> {
    validate_import(provenance)?;
    let existing = connection
        .query_row(
            "SELECT payload_fingerprint FROM import_provenance \
             WHERE account_id=?1 AND source_table=?2 AND target_key=?3",
            params![
                provenance.account_id.get(),
                provenance.source_table,
                provenance.target_key
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error)?;
    if let Some(existing) = existing {
        if existing == provenance.payload_fingerprint {
            return Ok(false);
        }
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "Pulse import logical row conflicts with previously imported content",
        ));
    }
    connection
        .execute(
            "INSERT INTO import_provenance (account_id, source_fingerprint, source_table, \
             source_row_id, target_key, payload_fingerprint, imported_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                provenance.account_id.get(),
                provenance.source_fingerprint,
                provenance.source_table,
                provenance.source_row_id,
                provenance.target_key,
                provenance.payload_fingerprint,
                provenance.imported_at.epoch_millis()
            ],
        )
        .map_err(sql_error)?;
    Ok(true)
}

fn upsert_import_machine(connection: &Connection, machine: &Machine) -> PulseResult<()> {
    if machine.last_seen < machine.first_seen {
        return Err(PulseError::invalid_input(
            "machine last_seen cannot precede first_seen",
        ));
    }
    connection
        .execute(
            "INSERT INTO machines (account_id, name, first_seen_ms, last_seen_ms) \
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT(account_id, name) DO UPDATE SET \
             first_seen_ms = MIN(machines.first_seen_ms, excluded.first_seen_ms), \
             last_seen_ms = MAX(machines.last_seen_ms, excluded.last_seen_ms)",
            params![
                machine.account_id.get(),
                machine.name.as_str(),
                machine.first_seen.epoch_millis(),
                machine.last_seen.epoch_millis()
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn validate_ingest_scope(
    account_id: AccountId,
    machine: &MachineName,
    batch: &IngestBatch,
    limits: IngestLimits,
) -> PulseResult<()> {
    if batch.row_count() > limits.max_rows_per_request {
        return Err(PulseError::invalid_input(
            "ingest request exceeds its row limit",
        ));
    }
    let snapshot_scope = batch
        .snapshots
        .iter()
        .all(|row| row.account_id == account_id && &row.machine == machine);
    let token_scope = batch
        .token_grains
        .iter()
        .all(|row| row.account_id == account_id && &row.machine == machine);
    let context_scope = batch
        .context_sessions
        .iter()
        .all(|row| row.account_id == account_id && &row.machine == machine);
    let gemini_scope = batch
        .gemini_quotas
        .iter()
        .all(|row| row.account_id == account_id);
    let profile_scope = batch
        .profiles
        .iter()
        .all(|row| row.account_id == account_id && row.origin == ProfileOrigin::Reported);
    if !(profile_scope && snapshot_scope && token_scope && context_scope && gemini_scope) {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "ingest rows must match the token-authoritative account and machine",
        ));
    }
    for profile in &batch.profiles {
        profile.validate()?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn enforce_ingest_caps(
    connection: &Connection,
    account_id: AccountId,
    batch: &IngestBatch,
    limits: IngestLimits,
) -> PulseResult<()> {
    let profile_count = account_row_count(connection, "profiles", account_id)?;
    let token_count = account_row_count(connection, "token_usage", account_id)?;
    let snapshot_count = account_row_count(connection, "usage_snapshots", account_id)?;
    let context_count = account_row_count(connection, "context_sessions", account_id)?;
    let gemini_count = account_row_count(connection, "gemini_quota", account_id)?;

    let mut profile_keys = HashSet::new();
    let mut new_profiles = 0_usize;
    for profile in &batch.profiles {
        if profile_keys.insert(profile.name.clone()) {
            let exists = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM profiles WHERE account_id=?1 AND name=?2)",
                    params![profile.account_id.get(), profile.name.as_str()],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(sql_error)?;
            new_profiles += usize::from(!exists);
        }
    }

    let mut token_keys = HashSet::new();
    let mut new_tokens = 0_usize;
    for grain in &batch.token_grains {
        let source = encode(&grain.source)?;
        let key = encode(&(
            grain.account_id,
            &grain.profile,
            &grain.machine,
            &grain.session_id,
            &grain.model,
            &grain.settings_hash,
            &grain.day,
            &source,
        ))?;
        if token_keys.insert(key) {
            let exists = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM token_usage WHERE account_id = ?1 AND profile = ?2 \
                     AND machine = ?3 AND session_id = ?4 AND model = ?5 AND settings_hash = ?6 \
                     AND day = ?7 AND source_json = ?8)",
                    params![
                        grain.account_id.get(),
                        grain.profile.as_str(),
                        grain.machine.as_str(),
                        grain.session_id.as_str(),
                        grain.model,
                        grain.settings_hash,
                        grain.day,
                        source
                    ],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(sql_error)?;
            new_tokens += usize::from(!exists);
        }
    }

    let mut context_keys = HashSet::new();
    let mut new_context = 0_usize;
    for session in &batch.context_sessions {
        let key = encode(&(
            session.account_id,
            &session.profile,
            &session.machine,
            &session.session_id,
        ))?;
        if context_keys.insert(key) {
            let exists = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM context_sessions WHERE account_id = ?1 \
                     AND profile = ?2 AND machine = ?3 AND session_id = ?4)",
                    params![
                        session.account_id.get(),
                        session.profile.as_str(),
                        session.machine.as_str(),
                        session.session_id.as_str()
                    ],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(sql_error)?;
            new_context += usize::from(!exists);
        }
    }

    let mut gemini_keys = HashSet::new();
    let mut new_gemini = 0_usize;
    for quota in &batch.gemini_quotas {
        if gemini_keys.insert(quota.model_id.clone()) {
            let exists = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM gemini_quota WHERE account_id = ?1 AND model_id = ?2)",
                    params![quota.account_id.get(), quota.model_id],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(sql_error)?;
            new_gemini += usize::from(!exists);
        }
    }

    if profile_count.saturating_add(new_profiles) > limits.max_profiles
        || snapshot_count.saturating_add(batch.snapshots.len()) > limits.max_usage_snapshots
        || token_count.saturating_add(new_tokens) > limits.max_token_rows
        || context_count.saturating_add(new_context) > limits.max_context_sessions
        || gemini_count.saturating_add(new_gemini) > limits.max_gemini_models
    {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "ingest would exceed an account row cap",
        ));
    }
    Ok(())
}

fn validate_replay(replay: &IngestReplay) -> PulseResult<()> {
    if replay.request_id.is_empty()
        || replay.request_id.len() > 128
        || !replay
            .request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(PulseError::invalid_input(
            "ingest request id must be a stable ASCII identifier",
        ));
    }
    validate_token_hash(&replay.payload_fingerprint)
}

fn ingest_batch_once(
    connection: &mut Connection,
    account_id: AccountId,
    machine: &MachineName,
    batch: &IngestBatch,
    limits: IngestLimits,
    replay: &IngestReplay,
) -> PulseResult<IdempotentIngestResult> {
    validate_ingest_scope(account_id, machine, batch, limits)?;
    validate_replay(replay)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let existing = transaction
        .query_row(
            "SELECT payload_fingerprint,snapshots,token_grains,context_sessions,gemini_quotas \
             FROM ingest_replays WHERE account_id=?1 AND machine=?2 AND request_id=?3",
            params![account_id.get(), machine.as_str(), replay.request_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    if let Some((fingerprint, snapshots, token_grains, context_sessions, gemini_quotas)) = existing
    {
        if fingerprint != replay.payload_fingerprint {
            return Err(PulseError::new(
                PulseErrorKind::Conflict,
                "ingest request id was reused with a different payload",
            ));
        }
        let result = IngestResult {
            snapshots: stored_count(snapshots)?,
            token_grains: stored_count(token_grains)?,
            context_sessions: stored_count(context_sessions)?,
            gemini_quotas: stored_count(gemini_quotas)?,
        };
        transaction.commit().map_err(sql_error)?;
        return Ok(IdempotentIngestResult {
            result,
            replayed: true,
        });
    }
    if account_row_count(&transaction, "ingest_replays", account_id)?
        >= MAX_INGEST_REPLAYS_PER_ACCOUNT
    {
        return Err(PulseError::new(
            PulseErrorKind::Conflict,
            "ingest replay keys reached the account cap",
        ));
    }
    enforce_ingest_caps(&transaction, account_id, batch, limits)?;
    for profile in &batch.profiles {
        upsert_reported_profile(&transaction, profile)?;
    }
    for snapshot in &batch.snapshots {
        insert_snapshot(&transaction, snapshot)?;
    }
    upsert_token_batch(&transaction, &batch.token_grains)?;
    for session in &batch.context_sessions {
        upsert_context(&transaction, session)?;
    }
    for quota in &batch.gemini_quotas {
        upsert_gemini(&transaction, quota)?;
    }
    let result = IngestResult {
        snapshots: batch.snapshots.len(),
        token_grains: batch.token_grains.len(),
        context_sessions: batch.context_sessions.len(),
        gemini_quotas: batch.gemini_quotas.len(),
    };
    transaction
        .execute(
            "INSERT INTO ingest_replays (account_id,machine,request_id,payload_fingerprint, \
             snapshots,token_grains,context_sessions,gemini_quotas,received_at_ms) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                account_id.get(),
                machine.as_str(),
                replay.request_id,
                replay.payload_fingerprint,
                i64::try_from(result.snapshots)
                    .map_err(|_| PulseError::invalid_input("snapshot result count is too large"))?,
                i64::try_from(result.token_grains)
                    .map_err(|_| PulseError::invalid_input("token result count is too large"))?,
                i64::try_from(result.context_sessions)
                    .map_err(|_| PulseError::invalid_input("context result count is too large"))?,
                i64::try_from(result.gemini_quotas)
                    .map_err(|_| PulseError::invalid_input("Gemini result count is too large"))?,
                replay.received_at.epoch_millis()
            ],
        )
        .map_err(sql_error)?;
    transaction.commit().map_err(sql_error)?;
    Ok(IdempotentIngestResult {
        result,
        replayed: false,
    })
}

fn stored_count(value: i64) -> PulseResult<usize> {
    usize::try_from(value).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "stored ingest replay count is invalid",
        )
    })
}

type RawIngestToken = (i64, i64, String, String, i64, Option<i64>, Option<i64>);

fn raw_ingest_token(row: &Row<'_>) -> rusqlite::Result<RawIngestToken> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn decode_ingest_token(raw: RawIngestToken) -> PulseResult<IngestToken> {
    Ok(IngestToken {
        id: raw.0,
        account_id: AccountId::new(raw.1)?,
        machine: MachineName::new(raw.2)?,
        token_hash: raw.3,
        created_at: instant(raw.4)?,
        last_used_at: raw.5.map(instant).transpose()?,
        revoked_at: raw.6.map(instant).transpose()?,
    })
}

fn validate_issued_token(
    machine: &Machine,
    token: &IngestToken,
    max_active_tokens: usize,
) -> PulseResult<()> {
    validate_token_hash(&token.token_hash)?;
    if token.id <= 0
        || machine.account_id != token.account_id
        || machine.name != token.machine
        || machine.last_seen < machine.first_seen
        || token.last_used_at.is_some()
        || token.revoked_at.is_some()
        || max_active_tokens == 0
        || max_active_tokens > MAX_QUERY_ROWS
    {
        return Err(PulseError::invalid_input(
            "atomic ingest token issuance input is invalid",
        ));
    }
    Ok(())
}

fn row_count(connection: &Connection, table: &str) -> PulseResult<usize> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let count = connection
        .query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    usize::try_from(count)
        .map_err(|_| PulseError::new(PulseErrorKind::Storage, "stored Pulse row count is invalid"))
}

fn account_row_count(
    connection: &Connection,
    table: &str,
    account_id: AccountId,
) -> PulseResult<usize> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE account_id = ?1");
    let count = connection
        .query_row(&sql, params![account_id.get()], |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    usize::try_from(count).map_err(|_| {
        PulseError::new(
            PulseErrorKind::Storage,
            "stored Pulse account row count is invalid",
        )
    })
}

#[cfg(all(test, unix))]
mod path_security_tests {
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt as _, symlink},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use sha2::{Digest as _, Sha256};

    use super::{SqliteOpenTestHook, SqliteStore, Store, install_sqlite_open_test_hook};
    use crate::pulse::{Account, AccountId, PulseErrorKind};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "atmux-sqlite-{label}-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .expect("private SQLite test root");
            Self(path)
        }

        fn directory(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .expect("private SQLite test directory");
            path
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn create_external_database(path: &Path) {
        let connection = rusqlite::Connection::open(path).expect("external SQLite target");
        connection
            .execute_batch(
                "CREATE TABLE sentinel(value TEXT NOT NULL); \
                 INSERT INTO sentinel VALUES ('unchanged')",
            )
            .expect("external sentinel schema");
    }

    fn file_digest(path: &Path) -> Vec<u8> {
        Sha256::digest(fs::read(path).expect("read SQLite target")).to_vec()
    }

    fn account() -> Account {
        Account {
            id: AccountId::new(1).expect("account id"),
            identity: "anchored@example.test".to_owned(),
            display_name: None,
        }
    }

    #[tokio::test]
    async fn pinned_open_rejects_ancestor_swap_without_touching_external_target() {
        let root = TestRoot::new("ancestor-swap");
        let safe = root.directory("safe");
        let moved = root.0.join("safe-moved");
        let external = root.directory("external");
        let external_database = external.join("pulse.sqlite3");
        create_external_database(&external_database);
        let before = file_digest(&external_database);
        let requested = safe.join("pulse.sqlite3");
        let hook_safe = safe.clone();
        let hook_moved = moved.clone();
        let hook_external = external.clone();
        install_sqlite_open_test_hook(
            requested.clone(),
            Box::new(move || {
                fs::rename(hook_safe, hook_moved).expect("move validated ancestor");
                symlink(hook_external, safe).expect("replace ancestor with symlink");
            }) as SqliteOpenTestHook,
        );

        let error = SqliteStore::open(&requested)
            .await
            .err()
            .expect("ancestor swap must fail closed");
        assert!(matches!(
            error.kind(),
            PulseErrorKind::Configuration | PulseErrorKind::Conflict
        ));
        assert_eq!(file_digest(&external_database), before);
    }

    #[tokio::test]
    async fn pinned_open_rejects_final_swap_without_touching_external_target() {
        let root = TestRoot::new("final-swap");
        let safe = root.directory("safe");
        let external = root.directory("external");
        let external_database = external.join("pulse.sqlite3");
        create_external_database(&external_database);
        let before = file_digest(&external_database);
        let requested = safe.join("pulse.sqlite3");
        let hook_requested = requested.clone();
        let hook_external = external_database.clone();
        install_sqlite_open_test_hook(
            requested.clone(),
            Box::new(move || {
                fs::remove_file(&hook_requested).expect("remove validated final file");
                symlink(hook_external, hook_requested).expect("replace final file with symlink");
            }) as SqliteOpenTestHook,
        );

        let error = SqliteStore::open(&requested)
            .await
            .err()
            .expect("final swap must fail closed");
        assert!(matches!(
            error.kind(),
            PulseErrorKind::Configuration | PulseErrorKind::Conflict
        ));
        assert_eq!(file_digest(&external_database), before);
    }

    #[tokio::test]
    async fn retained_guard_rejects_replacement_and_normal_wal_reopens() {
        let root = TestRoot::new("retained-guard");
        let safe = root.directory("safe");
        let requested = safe.join("pulse.sqlite3");
        let store = SqliteStore::open(&requested).await.expect("anchored store");
        store
            .upsert_account(account())
            .await
            .expect("normal WAL write");
        store.checkpoint().await.expect("normal WAL checkpoint");
        drop(store);

        let store = SqliteStore::open(&requested)
            .await
            .expect("reopen anchored store");
        assert!(
            store
                .get_account(AccountId::new(1).expect("account id"))
                .await
                .expect("read reopened account")
                .is_some()
        );
        let moved = root.0.join("safe-moved");
        let external = root.directory("external");
        let external_database = external.join("pulse.sqlite3");
        create_external_database(&external_database);
        let before = file_digest(&external_database);
        fs::rename(&safe, &moved).expect("move live database ancestor");
        symlink(&external, &safe).expect("replace live ancestor");

        let write_error = store
            .upsert_account(Account {
                display_name: Some("must-not-write".to_owned()),
                ..account()
            })
            .await
            .expect_err("guarded write must reject path replacement");
        assert!(matches!(
            write_error.kind(),
            PulseErrorKind::Configuration | PulseErrorKind::Conflict
        ));
        assert!(store.checkpoint().await.is_err());
        assert_eq!(file_digest(&external_database), before);

        fs::remove_file(&safe).expect("remove replacement symlink");
        fs::rename(&moved, &safe).expect("restore anchored namespace before close");
        drop(store);
        let reopened = SqliteStore::open(&requested)
            .await
            .expect("reopen after namespace restore");
        assert_eq!(
            reopened
                .get_account(AccountId::new(1).expect("account id"))
                .await
                .expect("read protected account")
                .expect("protected account")
                .display_name,
            None
        );
    }
}
