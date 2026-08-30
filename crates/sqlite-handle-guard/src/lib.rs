//! Narrow safe wrapper for `SQLite`'s live main-file identity control.

use std::{error::Error, ffi::c_void, fmt, ptr};

use rusqlite::{Connection, ffi};

/// Failure returned when `SQLite` cannot inspect its live main database handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileControlError {
    code: i32,
}

impl FileControlError {
    /// Returns `SQLite`'s result code.
    #[must_use]
    pub const fn code(self) -> i32 {
        self.code
    }
}

impl fmt::Display for FileControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SQLite main-file identity control failed with code {}",
            self.code
        )
    }
}

impl Error for FileControlError {}

/// Reports whether `SQLite`'s live main database file was renamed, moved, or
/// deleted after `SQLite` opened it.
///
/// A non-OK or unsupported file control is returned as an error so callers can
/// fail closed rather than assuming the live handle is still anchored.
///
/// # Errors
///
/// Returns `SQLite`'s result code when the VFS cannot perform the identity
/// control.
pub fn main_database_has_moved(connection: &Connection) -> Result<bool, FileControlError> {
    let mut moved = 0_i32;
    // SAFETY: the borrowed connection remains alive for the call; `main` is a
    // static NUL-terminated schema name; and SQLite writes one `int` through
    // the valid, aligned `moved` pointer only for SQLITE_FCNTL_HAS_MOVED.
    let result = unsafe {
        ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_HAS_MOVED,
            ptr::from_mut(&mut moved).cast::<c_void>(),
        )
    };
    if result == ffi::SQLITE_OK {
        Ok(moved != 0)
    } else {
        Err(FileControlError { code: result })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use rusqlite::Connection;

    use super::main_database_has_moved;

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn reports_when_the_live_main_file_is_renamed() {
        let root = std::env::temp_dir().join(format!(
            "sqlite-handle-guard-{}-{}",
            std::process::id(),
            NEXT_DATABASE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create handle-guard test directory");
        let database = root.join("main.sqlite3");
        let moved = root.join("moved.sqlite3");
        let connection = Connection::open(&database).expect("open test database");
        connection
            .execute_batch("CREATE TABLE sentinel(value INTEGER NOT NULL);")
            .expect("initialize test database");
        assert!(!main_database_has_moved(&connection).expect("inspect stable handle"));

        fs::rename(&database, &moved).expect("rename live database");
        assert!(main_database_has_moved(&connection).expect("inspect moved handle"));
        drop(connection);
        fs::remove_dir_all(root).expect("remove handle-guard test directory");
    }
}
