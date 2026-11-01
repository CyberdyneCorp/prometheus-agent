//! SQLite databases are copied page by page through SQLite's online backup
//! API, never as raw files: a live writer in WAL mode keeps writing while
//! the copy gets one consistent snapshot, and the copy has no `-wal`/`-shm`
//! sidecars.
//!
//! This works on database files by path: backup and restore copy whole
//! homes and profiles, below the session store (which is opened only
//! through a `ProfileScope`).

use std::path::Path;
use std::time::Duration;

use pmt_core::error::{Error, codes};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};

/// How often a busy source or target is retried before giving up.
const BUSY_RETRIES: u32 = 50;
const BUSY_PAUSE: Duration = Duration::from_millis(100);

/// Copies the database at `src` into a new file `dst` (one consistent
/// snapshot, even while another process writes `src`).
///
/// # Errors
///
/// `backup.io` when either database cannot be opened or copied.
pub fn copy(src: &Path, dst: &Path) -> Result<(), Error> {
    let source = Connection::open_with_flags(
        src,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| db_error(src, &e))?;
    let mut target = Connection::open(dst).map_err(|e| db_error(dst, &e))?;
    transfer(&source, &mut target).map_err(|e| db_error(src, &e))?;
    drop(target);
    crate::fsutil::private_file(dst);
    Ok(())
}

/// Replaces the contents of the live database `dst` with `src` through the
/// backup API, so open connections see the restored pages instead of a
/// file swapped under them. A missing `dst` is created.
///
/// # Errors
///
/// `backup.io`.
pub fn restore(src: &Path, dst: &Path) -> Result<(), Error> {
    let source = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| db_error(src, &e))?;
    let mut target = Connection::open(dst).map_err(|e| db_error(dst, &e))?;
    transfer(&source, &mut target).map_err(|e| db_error(dst, &e))?;
    drop(target);
    crate::fsutil::private_file(dst);
    Ok(())
}

fn transfer(source: &Connection, target: &mut Connection) -> rusqlite::Result<()> {
    let backup = Backup::new(source, target)?;
    let mut busy = 0;
    loop {
        // -1 copies every page in one step: one read transaction, so one
        // consistent snapshot of the source.
        match backup.step(-1)? {
            StepResult::Done => return Ok(()),
            StepResult::More => {}
            StepResult::Busy | StepResult::Locked if busy < BUSY_RETRIES => {
                busy += 1;
                std::thread::sleep(BUSY_PAUSE);
            }
            _ => {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    Some("database stayed busy".to_owned()),
                ));
            }
        }
    }
}

/// `PRAGMA integrity_check` returns `ok`.
#[must_use]
pub fn is_intact(path: &Path) -> bool {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|db| db.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)))
        .is_ok_and(|result| result == "ok")
}

/// Row counts of the `sessions` and `messages` tables (0 for a table that
/// does not exist yet), for the older-backup warning.
#[must_use]
pub fn activity(path: &Path) -> (u64, u64) {
    let Ok(db) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return (0, 0);
    };
    let count = |table: &str| -> u64 {
        db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .map_or(0, |n| u64::try_from(n).unwrap_or(0))
    };
    (count("sessions"), count("messages"))
}

fn db_error(path: &Path, e: &rusqlite::Error) -> Error {
    Error::new(&codes::BACKUP_IO, format!("{}: {e}", path.display()))
}
