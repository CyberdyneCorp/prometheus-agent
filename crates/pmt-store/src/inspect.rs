//! Read-only peeks at a profile's `state.db` that work whatever its schema
//! version, without migrating, checking integrity or binding the profile.
//! `prometheus update --rollback` uses them to decide whether the previous
//! binary can still read each profile (spec: distribution-and-update,
//! Rollback).

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use pmt_config::ProfileDir;
use pmt_core::error::{Error, codes};

use crate::location::DATABASE_FILE;

/// The `store_meta.schema_version` of `profile`'s `state.db`; `None` when
/// the profile has no store yet.
///
/// # Errors
///
/// `store.sqlite` when the file exists but cannot be read.
pub fn schema_version(profile: &ProfileDir) -> Result<Option<u32>, Error> {
    let Some(conn) = open(profile)? else {
        return Ok(None);
    };
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM store_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| sqlite(profile, &e))?;
    Ok(value.and_then(|v| v.parse().ok()))
}

/// How many sessions of `profile` started at or after `since_ms`
/// (Unix milliseconds); 0 when the profile has no store.
///
/// # Errors
///
/// `store.sqlite` when the file exists but cannot be read.
pub fn sessions_created_since(profile: &ProfileDir, since_ms: i64) -> Result<u64, Error> {
    let Some(conn) = open(profile)? else {
        return Ok(0);
    };
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM sessions WHERE started_at >= ?1",
            [since_ms],
            |row| row.get(0),
        )
        .map_err(|e| sqlite(profile, &e))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// The newest sessions of the profile at `profile_dir` (at most `limit`,
/// newest activity first) whose id or title starts with `prefix`, as
/// `(id, title)`; empty when the profile has no store. Shell completion
/// uses it: it never writes, migrates or creates anything.
///
/// # Errors
///
/// `store.sqlite` when the file exists but cannot be read.
pub fn session_refs(
    profile_dir: &std::path::Path,
    prefix: &str,
    limit: u32,
) -> Result<Vec<(String, Option<String>)>, Error> {
    let path = profile_dir.join(DATABASE_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let failed = |e: &rusqlite::Error| {
        Error::new(
            &codes::STORE_SQLITE,
            format!("cannot read {}: {e}", path.display()),
        )
    };
    // Without a `-wal` file every committed row is in the main file, and
    // `immutable=1` reads it without creating `-wal`/`-shm` files. With
    // one (a writer has it open), a plain read-only open sees the rows
    // still in the log and creates nothing new.
    let wal = path.with_file_name(format!("{DATABASE_FILE}-wal"));
    let uri = if wal.exists() {
        path.to_string_lossy().into_owned()
    } else {
        immutable_uri(&path)
    };
    let conn = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| failed(&e))?;
    let pattern = format!(
        "{}%",
        prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let mut statement = conn
        .prepare(
            "SELECT id, title FROM sessions \
             WHERE id LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\' \
             ORDER BY last_activity_at DESC LIMIT ?2",
        )
        .map_err(|e| failed(&e))?;
    let rows = statement
        .query_map(rusqlite::params![pattern, limit], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(|e| failed(&e))?;
    rows.collect::<Result<_, _>>().map_err(|e| failed(&e))
}

/// `file:<path>?immutable=1`, with the URI's reserved characters escaped.
fn immutable_uri(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let escaped = text
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    let slash = if escaped.starts_with('/') { "" } else { "/" };
    format!("file:{slash}{escaped}?immutable=1")
}

fn open(profile: &ProfileDir) -> Result<Option<Connection>, Error> {
    let path = profile.path().join(DATABASE_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map(Some)
    .map_err(|e| sqlite(profile, &e))
}

fn sqlite(profile: &ProfileDir, e: &rusqlite::Error) -> Error {
    Error::new(
        &codes::STORE_SQLITE,
        format!(
            "cannot read {}: {e}",
            profile.path().join(DATABASE_FILE).display()
        ),
    )
}
