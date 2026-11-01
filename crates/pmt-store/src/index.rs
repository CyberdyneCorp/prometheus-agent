//! Derived full-text indexes (spec: session-storage, Derived indexes fail
//! open and rebuild offline).
//!
//! `messages_fts` (words) and `messages_trigram` (CJK and short terms) hold
//! each message's text under the message's rowid. Canonical rows are the
//! truth: index maintenance runs inside the canonical write's transaction
//! but in a savepoint, and any index failure rolls back only the index
//! change, sets `store_meta.index_stale = 1` (which stops further index
//! maintenance, the "disabled triggers") and lets the canonical write
//! commit. Searches then scan canonical rows until
//! `prometheus sessions repair --indexes` (or `doctor --fix`) rebuilds the
//! indexes offline; the live write path never rebuilds them.

use pmt_core::doctor::{Check, Status};
use pmt_core::error::Error;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::blobs::{self, BlobRef};
use crate::error::{corrupt, sql};
use crate::store::Store;

/// The two index tables, as created by schema v2.
const INDEX_DDL: &str = "\
    CREATE VIRTUAL TABLE messages_fts USING fts5(text, tokenize = 'unicode61 remove_diacritics 2');\
    CREATE VIRTUAL TABLE messages_trigram USING fts5(text, tokenize = 'trigram');";

/// Index table names.
pub(crate) const WORD_INDEX: &str = "messages_fts";
pub(crate) const TRIGRAM_INDEX: &str = "messages_trigram";

/// Whether the indexes are marked stale (searches use the canonical scan).
pub(crate) fn is_stale(conn: &Connection) -> Result<bool, Error> {
    let flag: Option<String> = conn
        .query_row(
            "SELECT value FROM store_meta WHERE key = 'index_stale'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql)?;
    Ok(flag.as_deref() == Some("1"))
}

fn set_stale(tx: &Transaction<'_>, stale: bool) -> Result<(), Error> {
    tx.execute(
        "INSERT INTO store_meta (key, value) VALUES ('index_stale', ?1) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [if stale { "1" } else { "0" }],
    )
    .map(drop)
    .map_err(sql)
}

/// Runs index maintenance in a savepoint unless the indexes are stale; on
/// failure undoes it and marks the indexes stale instead of failing.
fn guarded(
    tx: &Transaction<'_>,
    change: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<()>,
) -> Result<(), Error> {
    if is_stale(tx)? {
        return Ok(());
    }
    tx.execute_batch("SAVEPOINT index_maintenance")
        .map_err(sql)?;
    match change(tx) {
        Ok(()) => tx.execute_batch("RELEASE index_maintenance").map_err(sql),
        Err(e) => {
            tx.execute_batch("ROLLBACK TO index_maintenance; RELEASE index_maintenance")
                .map_err(sql)?;
            tracing::warn!(
                "state.db search index failed ({e}); searches use a canonical scan until `prometheus sessions repair --indexes`"
            );
            set_stale(tx, true)
        }
    }
}

/// Indexes the text of newly inserted messages (`(rowid, text)`).
pub(crate) fn add(tx: &Transaction<'_>, entries: &[(i64, &str)]) -> Result<(), Error> {
    if entries.is_empty() {
        return Ok(());
    }
    guarded(tx, |tx| {
        for table in [WORD_INDEX, TRIGRAM_INDEX] {
            let mut stmt = tx.prepare_cached(&format!(
                "INSERT INTO {table} (rowid, text) VALUES (?1, ?2)"
            ))?;
            for (rowid, text) in entries {
                stmt.execute(params![rowid, text])?;
            }
        }
        Ok(())
    })
}

/// Drops the index entries of every message of `session`.
pub(crate) fn remove_session(tx: &Transaction<'_>, session: &str) -> Result<(), Error> {
    guarded(tx, |tx| {
        for table in [WORD_INDEX, TRIGRAM_INDEX] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE rowid IN \
                     (SELECT rowid FROM messages WHERE session_id = ?1)"
                ),
                [session],
            )?;
        }
        Ok(())
    })
}

/// Whether both index tables exist and answer a query (a read-only
/// probe: FTS5's integrity-check command is a write).
fn healthy(conn: &Connection) -> bool {
    [WORD_INDEX, TRIGRAM_INDEX].iter().all(|table| {
        conn.query_row(
            &format!("SELECT count(*) FROM {table} WHERE {table} MATCH '\"probe\"'"),
            [],
            |row| row.get::<_, i64>(0),
        )
        .is_ok()
    })
}

impl Store {
    /// Whether searches currently fall back to the canonical scan.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn indexes_stale(&self) -> Result<bool, Error> {
        self.read(is_stale)
    }

    /// Rebuilds both search indexes from the canonical rows (spilled
    /// contents included) and clears `index_stale`. Offline maintenance:
    /// `prometheus sessions repair --indexes` and `doctor --fix`.
    ///
    /// # Errors
    ///
    /// `store.schema_too_new`, `store.blob_missing`, `store.blob_corrupt`,
    /// `store.io`, `store.sqlite`.
    pub fn repair_indexes(&self) -> Result<u64, Error> {
        let blob_dir = self.location().blobs().to_path_buf();
        self.write(|tx| {
            tx.execute_batch(&format!(
                "DROP TABLE IF EXISTS {WORD_INDEX}; DROP TABLE IF EXISTS {TRIGRAM_INDEX}; {INDEX_DDL}"
            ))
            .map_err(sql)?;
            let mut indexed = 0;
            let mut stmt = tx
                .prepare("SELECT rowid, content, content_ref FROM messages")
                .map_err(sql)?;
            let mut rows = stmt.query([]).map_err(sql)?;
            while let Some(row) = rows.next().map_err(sql)? {
                let rowid: i64 = row.get(0).map_err(sql)?;
                let text = match (row.get::<_, Option<String>>(1).map_err(sql)?, row.get::<_, Option<String>>(2).map_err(sql)?) {
                    (Some(text), _) => text,
                    (None, Some(reference)) => {
                        let blob = BlobRef::parse(&reference)
                            .ok_or_else(|| corrupt("content_ref", &reference))?;
                        String::from_utf8_lossy(&blobs::get(&blob_dir, &blob)?).into_owned()
                    }
                    (None, None) => continue,
                };
                for table in [WORD_INDEX, TRIGRAM_INDEX] {
                    tx.execute(
                        &format!("INSERT INTO {table} (rowid, text) VALUES (?1, ?2)"),
                        params![rowid, text],
                    )
                    .map_err(sql)?;
                }
                indexed += 1;
            }
            set_stale(tx, false)?;
            Ok(indexed)
        })
    }

    /// The `store` doctor group's index and quarantine checks.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.io`.
    pub fn doctor_checks(&self) -> Result<Vec<Check>, Error> {
        let stale = self.indexes_stale()?;
        let healthy = self.read(|conn| Ok(healthy(conn)))?;
        let indexes = if stale || !healthy {
            Check::new(
                DOCTOR_GROUP,
                "search_indexes",
                Status::Warn,
                "the session search indexes are stale or damaged; searches scan every message",
            )
            .with_hint("run `prometheus doctor --fix` or `prometheus sessions repair --indexes`")
        } else {
            Check::new(
                DOCTOR_GROUP,
                "search_indexes",
                Status::Ok,
                "search indexes are current",
            )
        };
        let mut checks = vec![indexes];
        for report in crate::integrity::quarantine_reports(self.location().quarantine())? {
            checks.push(
                Check::new(
                    DOCTOR_GROUP,
                    "quarantine",
                    Status::Warn,
                    format!(
                        "state.db was quarantined to {} at {} ({}): {} rows salvaged, {} lost",
                        report.quarantined.display(),
                        report.at,
                        report.reason,
                        report.salvaged_rows(),
                        report.lost_rows()
                    ),
                )
                .with_hint("keep the quarantined file until the salvaged sessions are checked"),
            );
        }
        Ok(checks)
    }

    /// The `store` group's `--fix` repair: rebuilds stale or damaged
    /// search indexes. Returns what it did, one line per repair.
    ///
    /// # Errors
    ///
    /// As [`Store::repair_indexes`].
    pub fn doctor_fix(&self) -> Result<Vec<String>, Error> {
        let needs_rebuild = self.indexes_stale()? || !self.read(|conn| Ok(healthy(conn)))?;
        if !needs_rebuild {
            return Ok(Vec::new());
        }
        let indexed = self.repair_indexes()?;
        Ok(vec![format!(
            "rebuilt the session search indexes ({indexed} messages)"
        )])
    }
}

/// The doctor group of the store checks.
pub const DOCTOR_GROUP: &str = "store";
