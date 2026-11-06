//! `cron_runs`: one row per occurrence or manual run (spec: scheduled-jobs,
//! At-most-once occurrence firing; Run history).
//!
//! A scheduled occurrence is claimed by inserting its row under the unique
//! key (job, scheduled instant) and advancing the job in the same
//! transaction, after checking that the job's `next_run_at` is still the
//! one the caller saw: of two triggers racing on one occurrence exactly one
//! claims it.

use pmt_core::error::Error;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{JobRow, job_not_found, load_job, save_job};
use crate::error::sql;
use crate::leases::Holder;
use crate::sessions::now_ms;
use crate::store::Store;

/// One `cron_runs` row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunRow {
    /// `run_<ULID>`.
    pub id: String,
    /// The job (the row outlives it).
    pub job_id: String,
    /// The job's name when it ran.
    pub job_name: String,
    /// The occurrence, Unix milliseconds (the claim time for manual runs).
    pub scheduled_at: i64,
    /// `schedule`, `manual` or `replay`.
    pub trigger: String,
    /// `on_time`, `late` or `missed`.
    pub lateness: Option<String>,
    /// Occurrences a catch-up run stands for.
    pub collapsed: u32,
    /// Occurrences a `skipped_late` record stands for.
    pub skipped: u32,
    /// `queued`, `running` or a terminal status literal.
    pub status: String,
    /// Why (`idle`, `wall`, `previous run still active`, `unknown outcome`).
    pub reason: Option<String>,
    /// The failure.
    pub error: Option<String>,
    /// Execution start.
    pub started_at: Option<i64>,
    /// Execution end.
    pub ended_at: Option<i64>,
    /// The agent session.
    pub session_id: Option<String>,
    /// The output (inline up to 64 KiB).
    pub output: Option<String>,
    /// The blob holding a larger output.
    pub output_blob: Option<String>,
    /// The resolved inputs (JSON: prompt, skills, script output).
    pub inputs: Option<String>,
    /// The targets delivered to (JSON list).
    pub delivered: Option<String>,
    /// Input tokens of the run's session.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cost in USD.
    pub cost_usd: f64,
    /// The process executing the run.
    pub owner: Option<Holder>,
    /// Row creation.
    pub created_at: i64,
}

const COLUMNS: &str = "id, job_id, job_name, scheduled_at, trigger, lateness, collapsed, \
     skipped, status, reason, error, started_at, ended_at, session_id, output, output_blob, \
     inputs, delivered, input_tokens, output_tokens, cost_usd, pid, process_start, host_id, \
     created_at";

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<RunRow> {
    let pid: Option<u32> = row.get(21)?;
    let start: Option<i64> = row.get(22)?;
    let host: Option<String> = row.get(23)?;
    let owner = match (pid, start, host) {
        (Some(pid), Some(start), Some(host_id)) => Some(Holder {
            pid,
            process_start: u64::try_from(start).unwrap_or(0),
            host_id,
        }),
        _ => None,
    };
    Ok(RunRow {
        id: row.get(0)?,
        job_id: row.get(1)?,
        job_name: row.get(2)?,
        scheduled_at: row.get(3)?,
        trigger: row.get(4)?,
        lateness: row.get(5)?,
        collapsed: row.get(6)?,
        skipped: row.get(7)?,
        status: row.get(8)?,
        reason: row.get(9)?,
        error: row.get(10)?,
        started_at: row.get(11)?,
        ended_at: row.get(12)?,
        session_id: row.get(13)?,
        output: row.get(14)?,
        output_blob: row.get(15)?,
        inputs: row.get(16)?,
        delivered: row.get(17)?,
        input_tokens: u64::try_from(row.get::<_, i64>(18)?).unwrap_or(0),
        output_tokens: u64::try_from(row.get::<_, i64>(19)?).unwrap_or(0),
        cost_usd: row.get(20)?,
        owner,
        created_at: row.get(24)?,
    })
}

fn signed(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Inserts `run` unless its (job, instant) exists; `false` on conflict.
fn insert_run(conn: &Connection, run: &RunRow) -> Result<bool, Error> {
    let owner = run.owner.as_ref();
    let n = conn
        .execute(
            &format!(
                "INSERT OR IGNORE INTO cron_runs ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, \
                 ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, \
                 ?23, ?24, ?25)"
            ),
            params![
                run.id,
                run.job_id,
                run.job_name,
                run.scheduled_at,
                run.trigger,
                run.lateness,
                run.collapsed,
                run.skipped,
                run.status,
                run.reason,
                run.error,
                run.started_at,
                run.ended_at,
                run.session_id,
                run.output,
                run.output_blob,
                run.inputs,
                run.delivered,
                signed(run.input_tokens),
                signed(run.output_tokens),
                run.cost_usd,
                owner.map(|h| h.pid),
                owner.map(|h| signed(h.process_start)),
                owner.map(|h| h.host_id.clone()),
                run.created_at
            ],
        )
        .map_err(sql)?;
    Ok(n > 0)
}

fn save_run(conn: &Connection, run: &RunRow) -> Result<(), Error> {
    conn.execute("DELETE FROM cron_runs WHERE id = ?1", params![run.id])
        .map_err(sql)?;
    insert_run(conn, run).map(|_| ())
}

fn load_run(conn: &Connection, id: &str) -> Result<Option<RunRow>, Error> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM cron_runs WHERE id = ?1"),
        params![id],
        run_from_row,
    )
    .optional()
    .map_err(sql)
}

fn query_runs(
    conn: &Connection,
    filter: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<RunRow>, Error> {
    let mut stmt = conn
        .prepare(&format!("SELECT {COLUMNS} FROM cron_runs {filter}"))
        .map_err(sql)?;
    let rows = stmt.query_map(args, run_from_row).map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

/// The outcome of a claim.
#[derive(Debug, Clone, PartialEq)]
pub enum Claim {
    /// The run row was inserted and the job updated.
    Claimed(Box<JobRow>),
    /// Another trigger already claimed this occurrence (or moved the job
    /// on); nothing was written.
    Lost,
}

impl Store {
    /// Claims an occurrence: when job `job_id` is still enabled with
    /// `next_run_at == expected`, inserts `run` (unique per job and instant), lets
    /// `advance` update the job and commits both; otherwise writes
    /// nothing and answers [`Claim::Lost`].
    ///
    /// # Errors
    ///
    /// `cron.job_not_found`, the error of `advance`, `store.sqlite`.
    pub fn cron_claim(
        &self,
        job_id: &str,
        expected: Option<i64>,
        run: &RunRow,
        mut advance: impl FnMut(&mut JobRow) -> Result<(), Error>,
    ) -> Result<Claim, Error> {
        self.write(|tx| {
            let mut job = load_job(tx, job_id)?.ok_or_else(|| job_not_found(job_id))?;
            // A job paused (or moved on) since the caller looked is not fired.
            if !job.enabled || job.next_run_at != expected || !insert_run(tx, run)? {
                return Ok(Claim::Lost);
            }
            advance(&mut job)?;
            job.updated_at = now_ms();
            save_job(tx, &job)?;
            Ok(Claim::Claimed(Box::new(job)))
        })
    }

    /// Inserts a run without touching its job (manual and replay runs).
    ///
    /// # Errors
    ///
    /// `store.constraint` when the (job, instant) pair exists,
    /// `store.sqlite`.
    pub fn cron_insert_run(&self, run: &RunRow) -> Result<(), Error> {
        self.write(|tx| {
            if insert_run(tx, run)? {
                Ok(())
            } else {
                Err(Error::new(
                    &pmt_core::error::codes::STORE_CONSTRAINT,
                    format!("a run of job {} at {} exists", run.job_id, run.scheduled_at),
                ))
            }
        })
    }

    /// One run.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_run(&self, id: &str) -> Result<Option<RunRow>, Error> {
        self.read(|conn| load_run(conn, id))
    }

    /// Runs of `job_id`, newest first, optionally of one status.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_runs(
        &self,
        job_id: &str,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunRow>, Error> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.read(|conn| match status {
            Some(status) => query_runs(
                conn,
                "WHERE job_id = ?1 AND status = ?2 ORDER BY scheduled_at DESC, id DESC LIMIT ?3",
                &[&job_id, &status, &limit],
            ),
            None => query_runs(
                conn,
                "WHERE job_id = ?1 ORDER BY scheduled_at DESC, id DESC LIMIT ?2",
                &[&job_id, &limit],
            ),
        })
    }

    /// Every run in one of `statuses`, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_runs_in(&self, statuses: &[&str]) -> Result<Vec<RunRow>, Error> {
        let list = statuses
            .iter()
            .map(|s| format!("'{}'", s.replace('\'', "")))
            .collect::<Vec<_>>()
            .join(", ");
        self.read(|conn| {
            query_runs(
                conn,
                &format!("WHERE status IN ({list}) ORDER BY created_at, id"),
                &[],
            )
        })
    }

    /// Updates run `id` with `change` and its job with `job` (when the job
    /// still exists) in one transaction; returns the stored run.
    ///
    /// # Errors
    ///
    /// `cron.run_not_found`, the errors of the closures, `store.sqlite`.
    pub fn cron_update_run(
        &self,
        id: &str,
        mut change: impl FnMut(&mut RunRow) -> Result<(), Error>,
        mut job: impl FnMut(&RunRow, &mut JobRow) -> Result<(), Error>,
    ) -> Result<RunRow, Error> {
        self.write(|tx| {
            let mut run = load_run(tx, id)?.ok_or_else(|| run_not_found(id))?;
            change(&mut run)?;
            save_run(tx, &run)?;
            if let Some(mut row) = load_job(tx, &run.job_id)? {
                job(&run, &mut row)?;
                row.updated_at = now_ms();
                save_job(tx, &row)?;
            }
            Ok(run)
        })
    }

    /// Deletes runs created before `before` (Unix ms) that ended; returns
    /// how many.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_delete_runs_before(&self, before: i64) -> Result<usize, Error> {
        const OLD: &str = "SELECT id FROM cron_runs WHERE created_at < ?1 \
             AND status NOT IN ('queued', 'running')";
        self.write(|tx| {
            tx.execute(
                &format!(
                    "DELETE FROM blob_refs WHERE owner_kind = 'cron_run' AND owner_id IN ({OLD})"
                ),
                params![before],
            )
            .map_err(sql)?;
            tx.execute(
                &format!("DELETE FROM cron_runs WHERE id IN ({OLD})"),
                params![before],
            )
            .map_err(sql)
        })
    }
}

/// `cron.run_not_found` naming `id`.
#[must_use]
pub fn run_not_found(id: &str) -> Error {
    Error::new(
        &pmt_core::error::codes::CRON_RUN_NOT_FOUND,
        format!("no cron run '{id}'"),
    )
    .with_hint("`prometheus cron runs <job>` lists a job's runs")
}
