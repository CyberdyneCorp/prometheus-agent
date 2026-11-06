//! Scheduled jobs (spec: scheduled-jobs, Job record and storage): the
//! `cron_jobs` rows, their runs ([`runs`]), the delivery outbox
//! ([`deliveries`]) and the per-profile tick lease ([`lease`]).
//!
//! The store keeps the job's definition as opaque JSON (`spec`, owned by
//! `pmt-cron`) beside the columns the scheduler queries. Every
//! read-modify-write is one `BEGIN IMMEDIATE` transaction, so a CLI pausing
//! one job and a daemon advancing another never lose either change.

pub mod deliveries;
pub mod lease;
pub mod runs;

use pmt_core::error::{Error, codes};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::sql;
use crate::sessions::now_ms;
use crate::store::Store;

pub use deliveries::DeliveryRow;
pub use lease::{CronLease, LeaseHolder, TickLease};
pub use runs::{Claim, RunRow};

/// One `cron_jobs` row.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRow {
    /// 12 hex characters.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The job definition (JSON owned by `pmt-cron`).
    pub spec: String,
    /// The creating chat (JSON), `api`, or none.
    pub origin: Option<String>,
    /// `scheduled`, `paused`, `running`, `completed` or `error`.
    pub state: String,
    /// Whether the scheduler fires it.
    pub enabled: bool,
    /// Next occurrence, Unix milliseconds.
    pub next_run_at: Option<i64>,
    /// Start of the last run, Unix milliseconds.
    pub last_run_at: Option<i64>,
    /// The last run's status literal.
    pub last_status: Option<String>,
    /// Why the job or its last run failed.
    pub last_error: Option<String>,
    /// Executed occurrences (`repeat.completed`).
    pub completed: u32,
    /// When it was paused.
    pub paused_at: Option<i64>,
    /// Why it was paused.
    pub paused_reason: Option<String>,
    /// A preflight alert was sent and no preflight has passed since.
    pub alerted: bool,
    /// When it reached `completed`.
    pub completed_at: Option<i64>,
    /// Creation time.
    pub created_at: i64,
    /// Last change.
    pub updated_at: i64,
}

const COLUMNS: &str = "id, name, spec, origin, state, enabled, next_run_at, last_run_at, \
     last_status, last_error, completed, paused_at, paused_reason, alerted, completed_at, \
     created_at, updated_at";

fn job_from_row(row: &Row<'_>) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: row.get(0)?,
        name: row.get(1)?,
        spec: row.get(2)?,
        origin: row.get(3)?,
        state: row.get(4)?,
        enabled: row.get(5)?,
        next_run_at: row.get(6)?,
        last_run_at: row.get(7)?,
        last_status: row.get(8)?,
        last_error: row.get(9)?,
        completed: row.get(10)?,
        paused_at: row.get(11)?,
        paused_reason: row.get(12)?,
        alerted: row.get(13)?,
        completed_at: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

pub(crate) fn load_job(conn: &Connection, id: &str) -> Result<Option<JobRow>, Error> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM cron_jobs WHERE id = ?1"),
        params![id],
        job_from_row,
    )
    .optional()
    .map_err(sql)
}

pub(crate) fn save_job(conn: &Connection, job: &JobRow) -> Result<(), Error> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO cron_jobs ({COLUMNS}) VALUES \
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"
        ),
        params![
            job.id,
            job.name,
            job.spec,
            job.origin,
            job.state,
            job.enabled,
            job.next_run_at,
            job.last_run_at,
            job.last_status,
            job.last_error,
            job.completed,
            job.paused_at,
            job.paused_reason,
            job.alerted,
            job.completed_at,
            job.created_at,
            job.updated_at
        ],
    )
    .map_err(sql)?;
    Ok(())
}

/// `cron.job_not_found` naming `id`.
#[must_use]
pub fn job_not_found(id: &str) -> Error {
    Error::new(&codes::CRON_JOB_NOT_FOUND, format!("no cron job '{id}'"))
        .with_hint("`prometheus cron list --all` shows every job")
}

impl Store {
    /// Inserts a new job.
    ///
    /// # Errors
    ///
    /// `store.constraint` for a duplicate id, `store.sqlite`.
    pub fn cron_insert_job(&self, job: &JobRow) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                &format!(
                    "INSERT INTO cron_jobs ({COLUMNS}) VALUES \
                     (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"
                ),
                params![
                    job.id,
                    job.name,
                    job.spec,
                    job.origin,
                    job.state,
                    job.enabled,
                    job.next_run_at,
                    job.last_run_at,
                    job.last_status,
                    job.last_error,
                    job.completed,
                    job.paused_at,
                    job.paused_reason,
                    job.alerted,
                    job.completed_at,
                    job.created_at,
                    job.updated_at
                ],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// One job.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_job(&self, id: &str) -> Result<Option<JobRow>, Error> {
        self.read(|conn| load_job(conn, id))
    }

    /// Every job, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_jobs(&self) -> Result<Vec<JobRow>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM cron_jobs ORDER BY created_at, id"
                ))
                .map_err(sql)?;
            let rows = stmt.query_map([], job_from_row).map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })
    }

    /// Reads job `id`, lets `change` edit it and writes it back, all in
    /// one write transaction; `change` may run again when the transaction
    /// is retried. Returns the stored row.
    ///
    /// # Errors
    ///
    /// `cron.job_not_found`, the error of `change` (nothing is written),
    /// `store.sqlite`.
    pub fn cron_modify_job(
        &self,
        id: &str,
        mut change: impl FnMut(&mut JobRow) -> Result<(), Error>,
    ) -> Result<JobRow, Error> {
        self.write(|tx| {
            let mut job = load_job(tx, id)?.ok_or_else(|| job_not_found(id))?;
            change(&mut job)?;
            job.updated_at = now_ms();
            save_job(tx, &job)?;
            Ok(job)
        })
    }

    /// Deletes job `id` (its runs stay under run retention); `false` when
    /// there was none.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_delete_job(&self, id: &str) -> Result<bool, Error> {
        self.write(|tx| {
            let n = tx
                .execute("DELETE FROM cron_jobs WHERE id = ?1", params![id])
                .map_err(sql)?;
            Ok(n > 0)
        })
    }

    /// Deletes jobs `completed` before `before` (Unix ms); returns how
    /// many.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_delete_completed_before(&self, before: i64) -> Result<usize, Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM cron_jobs WHERE state = 'completed' AND completed_at IS NOT NULL \
                 AND completed_at < ?1",
                params![before],
            )
            .map_err(sql)
        })
    }
}
