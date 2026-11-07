//! Receipts of ended background processes (spec: execution-environments,
//! Retained process receipts).
//!
//! A receipt belongs to the conversation of the session that ran the
//! process: its lineage root, which resumptions and compression
//! continuations share. Reads are always scoped to that root, so an exact
//! id from another conversation finds nothing.

use std::time::Duration;

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{Row, params};

use crate::error::sql;
use crate::sessions::now_ms;
use crate::store::Store;

/// Receipts kept per profile.
pub const MAX_RECEIPTS: i64 = 64;
/// How long a receipt is kept.
pub const RETENTION: Duration = Duration::from_hours(7 * 24);

/// What an ended background process left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessReceipt {
    /// The process id the model knew it by.
    pub id: String,
    /// Its command line.
    pub command: String,
    /// Its exit code (-1 when killed or lost).
    pub exit_code: i32,
    /// Start, Unix milliseconds.
    pub started_at: i64,
    /// End, Unix milliseconds.
    pub ended_at: i64,
    /// The last characters of its redacted output.
    pub output: String,
}

const COLUMNS: &str = "id, command, exit_code, started_at, ended_at, output";

fn receipt_from_row(row: &Row<'_>) -> rusqlite::Result<ProcessReceipt> {
    Ok(ProcessReceipt {
        id: row.get(0)?,
        command: row.get(1)?,
        exit_code: row.get(2)?,
        started_at: row.get(3)?,
        ended_at: row.get(4)?,
        output: row.get(5)?,
    })
}

impl Store {
    /// Keeps `receipt` for `session`'s conversation, then drops receipts
    /// past [`RETENTION`] and all but the newest [`MAX_RECEIPTS`].
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.schema_too_new`, `store.sqlite`.
    pub fn keep_process_receipt(
        &self,
        session: SessionId,
        receipt: &ProcessReceipt,
    ) -> Result<(), Error> {
        let root = self.session(session)?.root_session_id.to_string();
        let oldest = now_ms() - i64::try_from(RETENTION.as_millis()).unwrap_or(i64::MAX);
        self.write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO process_receipts (id, session_id, root_session_id, \
                 command, exit_code, started_at, ended_at, output) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    receipt.id,
                    session.to_string(),
                    root,
                    receipt.command,
                    receipt.exit_code,
                    receipt.started_at,
                    receipt.ended_at,
                    receipt.output,
                ],
            )
            .map_err(sql)?;
            tx.execute(
                "DELETE FROM process_receipts WHERE ended_at < ?1 OR id NOT IN \
                 (SELECT id FROM process_receipts ORDER BY ended_at DESC, id DESC LIMIT ?2)",
                params![oldest, MAX_RECEIPTS],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// The receipts of `session`'s conversation whose id starts with
    /// `prefix` (with or without the `proc_` prefix), newest first.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn process_receipts_matching(
        &self,
        session: SessionId,
        prefix: &str,
    ) -> Result<Vec<ProcessReceipt>, Error> {
        let bare = prefix.strip_prefix("proc_").unwrap_or(prefix);
        Ok(self
            .process_receipts(session)?
            .into_iter()
            .filter(|r| {
                r.id.strip_prefix("proc_")
                    .unwrap_or(&r.id)
                    .starts_with(bare)
            })
            .collect())
    }

    /// Every unexpired receipt of `session`'s conversation, newest first.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn process_receipts(&self, session: SessionId) -> Result<Vec<ProcessReceipt>, Error> {
        let root = self.session(session)?.root_session_id.to_string();
        let oldest = now_ms() - i64::try_from(RETENTION.as_millis()).unwrap_or(i64::MAX);
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(&format!(
                    "SELECT {COLUMNS} FROM process_receipts \
                     WHERE root_session_id = ?1 AND ended_at >= ?2 ORDER BY ended_at DESC, id DESC"
                ))
                .map_err(sql)?;
            let rows = stmt
                .query_map(params![root, oldest], receipt_from_row)
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })
    }
}
