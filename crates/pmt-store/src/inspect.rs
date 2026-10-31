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
