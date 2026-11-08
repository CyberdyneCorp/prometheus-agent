//! Offline recovery and journal-mode control (spec: session-storage,
//! Offline recovery and journal mode). Both run with the profile's store
//! closed: the caller holds the home's writer lock, so no daemon or chat
//! has it open.
//!
//! Recovery never edits the original: every readable row of every table is
//! copied into a new clean store next to it, the derived indexes are
//! rebuilt, and [`swap_in`] replaces `state.db` only when asked, keeping
//! the original as `state.db.pre-recover`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_config::schema::JournalMode;
use pmt_core::error::{Error, codes};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use crate::error::sql;
use crate::integrity;
use crate::location::StoreLocation;
use crate::store::Store;

/// The `store_meta` key of a journal mode chosen with `sessions
/// journal-mode`.
pub const JOURNAL_MODE_KEY: &str = "journal_mode";

/// What a recovery copied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Recovered {
    /// The recovered database.
    pub path: PathBuf,
    /// Rows copied per table.
    pub recovered: BTreeMap<String, u64>,
    /// Rows that could not be read or written, per table.
    pub lost: BTreeMap<String, u64>,
}

/// Tables copied first (others reference them).
const FIRST: &[&str] = &["sessions", "messages", "usage_ledger", "blob_refs"];

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::io(&codes::STORE_IO, path, e)
}

/// Canonical tables of `conn`: no SQLite internals, no `store_meta`, no
/// FTS5 tables or their shadow tables (rebuilt instead).
fn tables(conn: &Connection) -> Result<Vec<String>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT name, sql FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' AND name <> 'store_meta' ORDER BY name",
        )
        .map_err(sql)?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    let virtuals: Vec<&str> = rows
        .iter()
        .filter(|(_, ddl)| ddl.starts_with("CREATE VIRTUAL"))
        .map(|(name, _)| name.as_str())
        .collect();
    let mut names: Vec<String> = rows
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| !virtuals.iter().any(|v| name.starts_with(v)))
        .collect();
    names.sort_by_key(|name| FIRST.iter().position(|f| f == name).unwrap_or(FIRST.len()));
    Ok(names)
}

/// Where a recovery made now is written by default.
#[must_use]
pub fn default_output(scope: &ProfileScope) -> PathBuf {
    let stamp = jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string();
    scope.path(format!("state.recovered-{stamp}.db"))
}

/// Copies every readable row of the profile's `state.db` into a new store
/// at `output` (which must not exist).
///
/// # Errors
///
/// `store.io` (the output exists, or files cannot be written), store
/// errors of the new store.
pub fn recover(scope: &ProfileScope, output: &Path) -> Result<Recovered, Error> {
    let original = StoreLocation::of(scope).database().to_path_buf();
    if output.exists() {
        return Err(Error::new(
            &codes::STORE_IO,
            format!("{} already exists; nothing was written", output.display()),
        ));
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let source = Connection::open_with_flags(&original, flags).map_err(sql)?;
    let store = Store::open_at(scope, output)?;
    let mut report = Recovered {
        path: output.to_path_buf(),
        ..Recovered::default()
    };
    let names = store.read(tables)?;
    store.write(|tx| {
        for table in &names {
            let (copied, lost) = integrity::copy_table(&source, tx, table)?;
            report.recovered.insert(table.clone(), copied);
            report.lost.insert(table.clone(), lost);
        }
        tx.execute_batch(
            "DELETE FROM memory_fts; \
             INSERT INTO memory_fts (content, entry_id) SELECT content, id FROM memory_entries;",
        )
        .map_err(sql)?;
        integrity::recount(tx)
    })?;
    store.repair_indexes()?;
    Ok(report)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Replaces the profile's `state.db` with `recovered`, keeping the
/// original (and its sidecars) as `state.db.pre-recover`.
///
/// # Errors
///
/// `store.io`.
pub fn swap_in(scope: &ProfileScope, recovered: &Path) -> Result<PathBuf, Error> {
    let database = StoreLocation::of(scope).database().to_path_buf();
    let kept = with_suffix(&database, ".pre-recover");
    std::fs::rename(&database, &kept).map_err(|e| io(&database, &e))?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = with_suffix(&database, suffix);
        if sidecar.exists() {
            let moved = with_suffix(&kept, suffix);
            std::fs::rename(&sidecar, &moved).map_err(|e| io(&sidecar, &e))?;
        }
    }
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(with_suffix(recovered, suffix));
    }
    std::fs::rename(recovered, &database).map_err(|e| io(recovered, &e))?;
    Ok(kept)
}

/// The journal mode recorded with `sessions journal-mode`, if any.
pub(crate) fn recorded_journal_mode(database: &Path) -> Option<JournalMode> {
    if !database.exists() {
        return None;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(database, flags).ok()?;
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM store_meta WHERE key = ?1",
            [JOURNAL_MODE_KEY],
            |row| row.get(0),
        )
        .optional()
        .ok()?;
    match value.as_deref() {
        Some("wal") => Some(JournalMode::Wal),
        Some("delete") => Some(JournalMode::Delete),
        _ => None,
    }
}

/// Switches the closed store to `mode` and records it in `store_meta`, so
/// every later open applies it (over `storage.journal_mode`).
///
/// # Errors
///
/// `store.sqlite`.
pub fn set_journal_mode(scope: &ProfileScope, mode: JournalMode) -> Result<(), Error> {
    let database = StoreLocation::of(scope).database().to_path_buf();
    let conn = Connection::open(&database).map_err(sql)?;
    let wanted = match mode {
        JournalMode::Wal => "wal",
        JournalMode::Delete => "delete",
    };
    conn.query_row(&format!("PRAGMA journal_mode = {wanted}"), [], |_| Ok(()))
        .map_err(sql)?;
    conn.execute(
        "INSERT INTO store_meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [JOURNAL_MODE_KEY, wanted],
    )
    .map_err(sql)?;
    Ok(())
}
