//! Forward-only migration runner.

use rusqlite::{Connection, TransactionBehavior, params};

use super::schema::{LATEST_SCHEMA_VERSION, MIGRATIONS};
use crate::pulse::error::{PulseError, PulseErrorKind, PulseResult};

const MIGRATION_TABLE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS pulse_schema_migrations (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    applied_at_ms INTEGER NOT NULL
) STRICT;
";

/// Applies every migration newer than the database's recorded version.
///
/// Each migration is committed independently and is never run in reverse.
///
/// # Errors
///
/// Returns a storage error if a migration fails, or a configuration error when
/// the database was created by a newer atmux build.
pub fn apply(connection: &mut Connection) -> PulseResult<u32> {
    connection
        .execute_batch(MIGRATION_TABLE_SQL)
        .map_err(storage_error)?;
    let current = current_version(connection)?;
    if current > LATEST_SCHEMA_VERSION {
        return Err(PulseError::configuration(format!(
            "Pulse database schema {current} is newer than supported version \
             {LATEST_SCHEMA_VERSION}"
        )));
    }

    for migration in MIGRATIONS.iter().filter(|item| item.version > current) {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        transaction
            .execute_batch(migration.sql)
            .map_err(storage_error)?;
        transaction
            .execute(
                "INSERT INTO pulse_schema_migrations (version, applied_at_ms) \
                 VALUES (?1, CAST(unixepoch('subsec') * 1000 AS INTEGER))",
                params![migration.version],
            )
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)?;
    }
    current_version(connection)
}

pub(crate) fn current_version(connection: &Connection) -> PulseResult<u32> {
    let version = connection
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM pulse_schema_migrations",
            [],
            |row| row.get::<_, u32>(0),
        )
        .map_err(storage_error)?;
    Ok(version)
}

#[allow(clippy::needless_pass_by_value)]
fn storage_error(error: rusqlite::Error) -> PulseError {
    PulseError::new(
        PulseErrorKind::Storage,
        format!("Pulse schema migration failed: {error}"),
    )
}
