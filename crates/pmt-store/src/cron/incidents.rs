//! `cron_incidents`: failed runs grouped by job and normalized error
//! signature (spec: scheduled-jobs, Failure incidents with alert-once and
//! acknowledge). The decision whether a failure is announced is
//! `pmt-cron`'s; this module stores the rows, each change one transaction.

use pmt_core::error::{Error, codes};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::sql;
use crate::store::Store;

/// One incident.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncidentRow {
    /// `inc_<ULID>`.
    pub id: String,
    /// The job.
    pub job_id: String,
    /// The job's name when it last failed.
    pub job_name: String,
    /// The normalized error.
    pub signature: String,
    /// `detected`, `alerted`, `resolved` or `closed`.
    pub state: String,
    /// The last error text (redacted, at most 2 KiB).
    pub error: String,
    /// Failures seen.
    pub occurrences: u32,
    /// The one reminder was sent.
    pub reminded: bool,
    /// First failure, Unix ms.
    pub first_seen_at: i64,
    /// Latest failure, Unix ms.
    pub last_seen_at: i64,
    /// When it was last announced.
    pub alerted_at: Option<i64>,
    /// Last change.
    pub updated_at: i64,
}

const COLUMNS: &str = "id, job_id, job_name, signature, state, error, occurrences, reminded, \
     first_seen_at, last_seen_at, alerted_at, updated_at";

fn from_row(row: &Row<'_>) -> rusqlite::Result<IncidentRow> {
    Ok(IncidentRow {
        id: row.get(0)?,
        job_id: row.get(1)?,
        job_name: row.get(2)?,
        signature: row.get(3)?,
        state: row.get(4)?,
        error: row.get(5)?,
        occurrences: row.get(6)?,
        reminded: row.get(7)?,
        first_seen_at: row.get(8)?,
        last_seen_at: row.get(9)?,
        alerted_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn save(conn: &Connection, row: &IncidentRow) -> Result<(), Error> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO cron_incidents ({COLUMNS}) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
        ),
        params![
            row.id,
            row.job_id,
            row.job_name,
            row.signature,
            row.state,
            row.error,
            row.occurrences,
            row.reminded,
            row.first_seen_at,
            row.last_seen_at,
            row.alerted_at,
            row.updated_at
        ],
    )
    .map_err(sql)?;
    Ok(())
}

fn select(
    conn: &Connection,
    filter: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<IncidentRow>, Error> {
    let mut stmt = conn
        .prepare(&format!("SELECT {COLUMNS} FROM cron_incidents {filter}"))
        .map_err(sql)?;
    let rows = stmt.query_map(args, from_row).map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

/// `cron.incident_not_found` naming `id`.
#[must_use]
pub fn incident_not_found(id: &str) -> Error {
    Error::new(
        &codes::CRON_INCIDENT_NOT_FOUND,
        format!("no cron incident '{id}'"),
    )
    .with_hint("`prometheus cron incidents` lists them")
}

impl Store {
    /// Reads the incident of (`job_id`, `signature`) (if any), lets
    /// `change` produce the row to store and stores it, in one write
    /// transaction; returns what `change` returned.
    ///
    /// # Errors
    ///
    /// The error of `change`, `store.sqlite`.
    pub fn cron_record_incident<T>(
        &self,
        job_id: &str,
        signature: &str,
        mut change: impl FnMut(Option<IncidentRow>) -> Result<(IncidentRow, T), Error>,
    ) -> Result<T, Error> {
        self.write(|tx| {
            let found = tx
                .query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM cron_incidents WHERE job_id = ?1 AND signature = ?2"
                    ),
                    params![job_id, signature],
                    from_row,
                )
                .optional()
                .map_err(sql)?;
            let (row, out) = change(found)?;
            save(tx, &row)?;
            Ok(out)
        })
    }

    /// Marks the open (`detected` or `alerted`) incidents of `job_id`
    /// `resolved`; returns how many.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_resolve_incidents(&self, job_id: &str, now: i64) -> Result<usize, Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE cron_incidents SET state = 'resolved', reminded = 0, updated_at = ?2 \
                 WHERE job_id = ?1 AND state IN ('detected', 'alerted')",
                params![job_id, now],
            )
            .map_err(sql)
        })
    }

    /// Incidents, newest activity first, optionally of one state and job.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_incidents(
        &self,
        state: Option<&str>,
        job_id: Option<&str>,
    ) -> Result<Vec<IncidentRow>, Error> {
        self.read(|conn| {
            select(
                conn,
                "WHERE (?1 IS NULL OR state = ?1) AND (?2 IS NULL OR job_id = ?2) \
                 ORDER BY updated_at DESC, id DESC",
                &[&state, &job_id],
            )
        })
    }

    /// Sets incident `id` `closed` (acknowledged); returns it.
    ///
    /// # Errors
    ///
    /// `cron.incident_not_found`, `store.sqlite`.
    pub fn cron_ack_incident(&self, id: &str, now: i64) -> Result<IncidentRow, Error> {
        self.write(|tx| {
            let mut rows = select(tx, "WHERE id = ?1", &[&id])?;
            let mut row = rows.pop().ok_or_else(|| incident_not_found(id))?;
            "closed".clone_into(&mut row.state);
            row.updated_at = now;
            save(tx, &row)?;
            Ok(row)
        })
    }
}
