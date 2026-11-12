//! Run records of the Runs API (spec: api-server, Runs API lifecycle).
//! A run's status only moves forward: once terminal it never changes, so
//! a late result cannot overwrite an `interrupted` written at shutdown.

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use rusqlite::{OptionalExtension, Row, params};

use super::admissions::{RETAIN_FOR, millis_of};
use crate::error::sql;
use crate::gateway::timestamp;
use crate::sessions::now_ms;
use crate::store::Store;

/// The terminal statuses.
pub const TERMINAL: &[&str] = &["completed", "failed", "cancelled", "interrupted"];

/// One run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRun {
    /// `run_<ulid>`.
    pub id: String,
    /// The session it runs in.
    pub session_id: String,
    /// `queued`, `running`, ..., `interrupted`, `unknown`.
    pub status: String,
    /// The admission that created it.
    pub admission_id: String,
    /// Bumped by every control the run asks for.
    pub execution_generation: i64,
    /// The daemon instance executing it.
    pub owner: String,
    /// What the request asked for (`{model, provider}` JSON).
    pub requested: Option<String>,
    /// The final answer.
    pub output: Option<String>,
    /// Usage JSON.
    pub usage: Option<String>,
    /// The `{provider, model, route_source}` that served it (JSON).
    pub runtime: Option<String>,
    /// Why it failed or stopped.
    pub error: Option<String>,
    /// When it was created.
    pub created_at: Timestamp,
    /// Its last status change.
    pub updated_at: Timestamp,
}

impl ApiRun {
    /// Whether the run reached a final status.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        TERMINAL.contains(&self.status.as_str())
    }
}

/// A change to a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunUpdate {
    /// The new status.
    pub status: Option<String>,
    /// A new execution generation.
    pub execution_generation: Option<i64>,
    /// The final answer.
    pub output: Option<String>,
    /// Usage JSON.
    pub usage: Option<String>,
    /// Runtime JSON.
    pub runtime: Option<String>,
    /// The error.
    pub error: Option<String>,
}

const COLUMNS: &str = "id, session_id, status, admission_id, execution_generation, owner, requested, \
     output, usage, runtime, error, created_at, updated_at";

fn run(row: &Row<'_>) -> rusqlite::Result<ApiRun> {
    Ok(ApiRun {
        id: row.get(0)?,
        session_id: row.get(1)?,
        status: row.get(2)?,
        admission_id: row.get(3)?,
        execution_generation: row.get(4)?,
        owner: row.get(5)?,
        requested: row.get(6)?,
        output: row.get(7)?,
        usage: row.get(8)?,
        runtime: row.get(9)?,
        error: row.get(10)?,
        created_at: timestamp(row.get(11)?),
        updated_at: timestamp(row.get(12)?),
    })
}

fn not_found(id: &str) -> Error {
    Error::new(&codes::SESSION_NOT_FOUND, format!("run {id} not found"))
}

impl Store {
    /// Records a new run.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn insert_api_run(&self, new: &ApiRun) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                &format!("INSERT INTO api_runs ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"),
                params![
                    new.id,
                    new.session_id,
                    new.status,
                    new.admission_id,
                    new.execution_generation,
                    new.owner,
                    new.requested,
                    new.output,
                    new.usage,
                    new.runtime,
                    new.error,
                    new.created_at.as_millisecond(),
                    new.updated_at.as_millisecond()
                ],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// The run `id`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn api_run(&self, id: &str) -> Result<Option<ApiRun>, Error> {
        self.read(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM api_runs WHERE id = ?1"),
                [id],
                run,
            )
            .optional()
            .map_err(sql)
        })
    }

    /// Applies `update` to run `id` unless it is terminal already;
    /// returns the run as it now is.
    ///
    /// # Errors
    ///
    /// `session.not_found` for an unknown run, `store.busy`, `store.sqlite`.
    pub fn update_api_run(&self, id: &str, update: &RunUpdate) -> Result<ApiRun, Error> {
        self.write(|tx| {
            let current = tx
                .query_row(
                    &format!("SELECT {COLUMNS} FROM api_runs WHERE id = ?1"),
                    [id],
                    run,
                )
                .optional()
                .map_err(sql)?
                .ok_or_else(|| not_found(id))?;
            if current.is_terminal() {
                return Ok(current);
            }
            tx.execute(
                "UPDATE api_runs SET status = coalesce(?2, status), \
                 execution_generation = coalesce(?3, execution_generation), \
                 output = coalesce(?4, output), usage = coalesce(?5, usage), \
                 runtime = coalesce(?6, runtime), error = coalesce(?7, error), updated_at = ?8 \
                 WHERE id = ?1",
                params![
                    id,
                    update.status,
                    update.execution_generation,
                    update.output,
                    update.usage,
                    update.runtime,
                    update.error,
                    now_ms()
                ],
            )
            .map_err(sql)?;
            tx.query_row(
                &format!("SELECT {COLUMNS} FROM api_runs WHERE id = ?1"),
                [id],
                run,
            )
            .map_err(sql)
        })
    }

    /// Every run not in a terminal status.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn active_api_runs(&self) -> Result<Vec<ApiRun>, Error> {
        self.read(|conn| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM api_runs WHERE status NOT IN \
                     ('completed', 'failed', 'cancelled', 'interrupted') ORDER BY created_at"
                ))
                .map_err(sql)?;
            let rows = statement.query_map([], run).map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })
    }

    /// Removes terminal runs whose last status change is older than
    /// [`RETAIN_FOR`].
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn prune_api_runs(&self, now: Timestamp) -> Result<usize, Error> {
        let before = now.as_millisecond() - millis_of(RETAIN_FOR);
        self.write(|tx| {
            tx.execute(
                "DELETE FROM api_runs WHERE updated_at < ?1 AND status IN \
                 ('completed', 'failed', 'cancelled', 'interrupted')",
                [before],
            )
            .map_err(sql)
        })
    }
}
