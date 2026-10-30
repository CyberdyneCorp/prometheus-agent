//! Startup integrity check, quarantine and salvage (spec: session-storage,
//! Integrity check and quarantine).
//!
//! The first writable open of a database in a process runs
//! `PRAGMA quick_check`. A failing file (or one a running store flagged
//! after a corruption error) is never repaired in place: it and its
//! sidecars move to `<profile>/quarantine/state-<timestamp>.db`, a fresh
//! `state.db` is created, every readable canonical row is copied into it
//! and the indexes are rebuilt. The salvage report (rows salvaged and lost
//! per table) is logged at ERROR and kept beside the quarantined file for
//! `prometheus doctor`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use pmt_core::error::Error;
use pmt_core::private_fs;
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::error::{self, sql};
use crate::files;
use crate::store::Store;

/// Canonical tables, parents first. Leases are not canonical (they lapse)
/// and message blob references are rebuilt by the messages trigger.
const CANONICAL_TABLES: &[&str] = &["sessions", "messages", "usage_ledger", "blob_refs"];

/// Databases already checked by this process.
static CHECKED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// What a quarantine salvaged, persisted as `state-<timestamp>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineReport {
    /// The quarantined database file.
    pub quarantined: PathBuf,
    /// When it was quarantined (RFC 3339).
    pub at: String,
    /// Why (`quick_check` output or the corruption error).
    pub reason: String,
    /// Rows copied into the fresh store, per table.
    pub salvaged: BTreeMap<String, u64>,
    /// Rows that could not be read or inserted, per table.
    pub lost: BTreeMap<String, u64>,
}

impl QuarantineReport {
    /// Rows lost over every table.
    #[must_use]
    pub fn lost_rows(&self) -> u64 {
        self.lost.values().sum()
    }

    /// Rows salvaged over every table.
    #[must_use]
    pub fn salvaged_rows(&self) -> u64 {
        self.salvaged.values().sum()
    }
}

/// A database moved to quarantine, waiting for its salvage.
pub(crate) struct Quarantined {
    path: PathBuf,
    reason: String,
}

/// The marker a running store leaves after a corruption error, so the next
/// open quarantines the file even when `quick_check` passes.
pub(crate) fn corrupt_marker(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".corrupt");
    PathBuf::from(name)
}

/// Runs the integrity check once per process and database; quarantines a
/// failing file. A store newer than `supported` is left alone (it opens
/// read-only and this build cannot judge it).
pub(crate) fn check_at_open(
    database: &Path,
    quarantine_dir: &Path,
    supported: u32,
) -> Result<Option<Quarantined>, Error> {
    let marker = corrupt_marker(database);
    let flagged = marker.exists();
    if !flagged && !first_check(database) {
        return Ok(None);
    }
    let empty = std::fs::metadata(database).is_ok_and(|m| m.len() == 0);
    let reason = if flagged {
        Some("a running store reported structural corruption".to_owned())
    } else if empty {
        None
    } else {
        problem(database, supported)
    };
    let Some(reason) = reason else {
        return Ok(None);
    };
    let path = quarantine(database, quarantine_dir)?;
    let _ = std::fs::remove_file(&marker);
    Ok(Some(Quarantined { path, reason }))
}

fn first_check(database: &Path) -> bool {
    let key = std::fs::canonicalize(database).unwrap_or_else(|_| database.to_path_buf());
    let mut checked = CHECKED.lock().unwrap_or_else(PoisonError::into_inner);
    checked.get_or_insert_with(HashSet::new).insert(key)
}

/// Why the file fails the check, `None` when it passes.
fn problem(database: &Path, supported: u32) -> Option<String> {
    let conn = match Connection::open(database) {
        Ok(conn) => conn,
        Err(e) => return Some(e.to_string()),
    };
    let version: Result<Option<String>, _> = conn
        .query_row(
            "SELECT value FROM store_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional();
    match version {
        Ok(Some(v)) if v.parse::<u32>().is_ok_and(|v| v > supported) => return None,
        Err(e) if !is_missing_table(&e) => return Some(e.to_string()),
        _ => {}
    }
    match conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0)) {
        Ok(result) if result == "ok" => None,
        Ok(result) => Some(format!("quick_check: {result}")),
        Err(e) => Some(format!("quick_check: {e}")),
    }
}

fn is_missing_table(e: &rusqlite::Error) -> bool {
    e.to_string().contains("no such table")
}

/// Moves the database and its sidecars to `quarantine_dir`.
fn quarantine(database: &Path, quarantine_dir: &Path) -> Result<PathBuf, Error> {
    files::private_dir(quarantine_dir)?;
    let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%S%.3fZ");
    let target = quarantine_dir.join(format!("state-{stamp}.db"));
    for suffix in ["", "-wal", "-shm"] {
        let from = with_suffix(database, suffix);
        if from.exists() {
            let to = with_suffix(&target, suffix);
            std::fs::rename(&from, &to).map_err(|e| error::io(&from, &e))?;
        }
    }
    Ok(target)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

impl Store {
    /// Copies every readable canonical row of `quarantined` into this
    /// (fresh) store, rebuilds the indexes and records the report.
    pub(crate) fn salvage(&self, quarantined: Quarantined) -> Result<QuarantineReport, Error> {
        let mut report = QuarantineReport {
            quarantined: quarantined.path.clone(),
            at: jiff::Timestamp::now().to_string(),
            reason: quarantined.reason,
            salvaged: BTreeMap::new(),
            lost: BTreeMap::new(),
        };
        if let Some(source) = open_source(&quarantined.path, &self.profile_id().to_string()) {
            self.write(|tx| {
                for table in CANONICAL_TABLES {
                    let (copied, lost) = copy_table(&source, tx, table)?;
                    report.salvaged.insert((*table).to_owned(), copied);
                    report.lost.insert((*table).to_owned(), lost);
                }
                recount(tx)
            })?;
        }
        self.repair_indexes()?;
        save_report(&report)?;
        tracing::error!(
            quarantined = %report.quarantined.display(),
            salvaged = report.salvaged_rows(),
            lost = report.lost_rows(),
            "state.db failed its integrity check ({}); moved to {}, salvaged {} rows, lost {}",
            report.reason,
            report.quarantined.display(),
            report.salvaged_rows(),
            report.lost_rows()
        );
        Ok(report)
    }
}

/// The quarantined file, when it may be salvaged into this profile: a file
/// whose readable `store_meta` names another profile is not.
fn open_source(path: &Path, profile_id: &str) -> Option<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags).ok()?;
    let owner: Option<String> = conn
        .query_row(
            "SELECT value FROM store_meta WHERE key = 'profile_id'",
            [],
            |row| row.get(0),
        )
        .ok();
    match owner {
        Some(owner) if owner != profile_id => {
            tracing::error!(
                "{} belongs to profile {owner}; not salvaged",
                path.display()
            );
            None
        }
        _ => Some(conn),
    }
}

/// Copies one table row by row; returns (copied, lost).
fn copy_table(source: &Connection, tx: &Transaction<'_>, table: &str) -> Result<(u64, u64), Error> {
    let Some(columns) = shared_columns(source, tx, table)? else {
        return Ok((0, 0));
    };
    let list = columns.join(", ");
    let placeholders = vec!["?"; columns.len()].join(", ");
    let insert = format!("INSERT OR IGNORE INTO {table} ({list}) VALUES ({placeholders})");
    let (rows, unreadable) = read_rows(source, table, &list, columns.len());
    let mut copied = 0;
    let mut refused = 0;
    for row in rows {
        match tx.execute(&insert, params_from_iter(row)) {
            Ok(1) => copied += 1,
            Ok(_) => {}
            Err(_) => refused += 1,
        }
    }
    Ok((copied, unreadable + refused))
}

/// Columns `table` has in both databases; `None` when the source has no
/// readable `table`.
fn shared_columns(
    source: &Connection,
    tx: &Transaction<'_>,
    table: &str,
) -> Result<Option<Vec<String>>, Error> {
    let Ok(theirs) = column_names(source, table) else {
        return Ok(None);
    };
    let ours = column_names(tx, table).map_err(sql)?;
    let shared: Vec<String> = theirs.into_iter().filter(|c| ours.contains(c)).collect();
    Ok((!shared.is_empty()).then_some(shared))
}

fn column_names(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    names.collect()
}

/// Every readable row of `table` and the count of unreadable ones. Rowid
/// tables are probed one rowid at a time so a damaged page loses only its
/// own rows; others are scanned until the first error.
fn read_rows(source: &Connection, table: &str, list: &str, width: usize) -> (Vec<Vec<Value>>, u64) {
    let read = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Vec<Value>> {
        (0..width).map(|i| row.get::<_, Value>(i)).collect()
    };
    let max: Option<i64> = source
        .query_row(&format!("SELECT max(rowid) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap_or(None);
    let mut rows = Vec::new();
    let mut failed = 0;
    if let Some(max) = max {
        let query = format!("SELECT {list} FROM {table} WHERE rowid = ?1");
        for rowid in 1..=max {
            match source.query_row(&query, [rowid], read).optional() {
                Ok(Some(row)) => rows.push(row),
                Ok(None) => {}
                Err(_) => failed += 1,
            }
        }
        return (rows, failed);
    }
    let Ok(mut stmt) = source.prepare(&format!("SELECT {list} FROM {table}")) else {
        return (rows, 1);
    };
    let Ok(mut cursor) = stmt.query([]) else {
        return (rows, 1);
    };
    loop {
        match cursor.next() {
            Ok(Some(row)) => match read(row) {
                Ok(values) => rows.push(values),
                Err(_) => failed += 1,
            },
            Ok(None) => break,
            Err(_) => {
                failed += 1;
                break;
            }
        }
    }
    (rows, failed)
}

/// Session counters follow the rows that survived.
fn recount(tx: &Transaction<'_>) -> Result<(), Error> {
    tx.execute_batch(
        "UPDATE sessions SET message_count = \
           (SELECT count(*) FROM messages m WHERE m.session_id = sessions.id)",
    )
    .map_err(sql)
}

fn save_report(report: &QuarantineReport) -> Result<(), Error> {
    let path = report.quarantined.with_extension("json");
    let text = serde_json::to_vec_pretty(report).expect("reports serialize");
    private_fs::write_atomic(&path, &text).map_err(|e| error::io(&path, &e))
}

/// Every salvage report in `quarantine_dir`, oldest first.
///
/// # Errors
///
/// `store.io` when the directory exists but cannot be read.
pub fn quarantine_reports(quarantine_dir: &Path) -> Result<Vec<QuarantineReport>, Error> {
    let entries = match std::fs::read_dir(quarantine_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(error::io(quarantine_dir, &e)),
    };
    let mut reports: Vec<QuarantineReport> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| std::fs::read(&path).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    reports.sort_by(|a, b| a.at.cmp(&b.at));
    Ok(reports)
}
