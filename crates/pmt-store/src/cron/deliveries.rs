//! `cron_deliveries`: the durable delivery outbox of job runs (spec:
//! scheduled-jobs, Durable delivery outbox). Each row is sent at most once
//! through `pending → sending → sent | failed | suppressed`; a row left in
//! `sending` by a sender that died becomes `unknown` and is never resent.

use pmt_core::error::Error;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::sql;
use crate::sessions::now_ms;
use crate::store::Store;

/// One outbox row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRow {
    /// `dlv_<ULID>`.
    pub id: String,
    /// The run it delivers.
    pub run_id: String,
    /// The run's job.
    pub job_id: String,
    /// The resolved target (`telegram`, `telegram:42:7`...).
    pub target: String,
    /// The framed, redacted text.
    pub body: String,
    /// A failure notice (muted where warnings are suppressed).
    pub warning: bool,
    /// `pending`, `sending`, `sent`, `failed`, `suppressed` or `unknown`.
    pub state: String,
    /// Send attempts so far.
    pub attempts: u32,
    /// Not before this, Unix milliseconds.
    pub next_attempt_at: i64,
    /// The last failure.
    pub error: Option<String>,
    /// Creation time.
    pub created_at: i64,
    /// Last change.
    pub updated_at: i64,
    /// The platform's id of the sent message (a `sent` row without one is
    /// unverified).
    pub message_id: Option<String>,
    /// The delivery opens a conversation a reply continues.
    pub continuable: bool,
}

const COLUMNS: &str = "id, run_id, job_id, target, body, warning, state, attempts, \
     next_attempt_at, error, created_at, updated_at, message_id, continuable";

fn from_row(row: &Row<'_>) -> rusqlite::Result<DeliveryRow> {
    Ok(DeliveryRow {
        id: row.get(0)?,
        run_id: row.get(1)?,
        job_id: row.get(2)?,
        target: row.get(3)?,
        body: row.get(4)?,
        warning: row.get(5)?,
        state: row.get(6)?,
        attempts: row.get(7)?,
        next_attempt_at: row.get(8)?,
        error: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        message_id: row.get(12)?,
        continuable: row.get(13)?,
    })
}

fn select(
    conn: &Connection,
    filter: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<DeliveryRow>, Error> {
    let mut stmt = conn
        .prepare(&format!("SELECT {COLUMNS} FROM cron_deliveries {filter}"))
        .map_err(sql)?;
    let rows = stmt.query_map(args, from_row).map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

impl Store {
    /// Queues `rows` (a row whose run and target exist is kept as is).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_enqueue(&self, rows: &[DeliveryRow]) -> Result<(), Error> {
        self.write(|tx| {
            for row in rows {
                tx.execute(
                    &format!(
                        "INSERT OR IGNORE INTO cron_deliveries ({COLUMNS}) VALUES \
                         (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
                    ),
                    params![
                        row.id,
                        row.run_id,
                        row.job_id,
                        row.target,
                        row.body,
                        row.warning,
                        row.state,
                        row.attempts,
                        row.next_attempt_at,
                        row.error,
                        row.created_at,
                        row.updated_at,
                        row.message_id,
                        row.continuable
                    ],
                )
                .map_err(sql)?;
            }
            Ok(())
        })
    }

    /// Takes the oldest `pending` row due at `now` (Unix ms): it becomes
    /// `sending` with one more attempt, committed before anything is sent.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_take_delivery(&self, now: i64) -> Result<Option<DeliveryRow>, Error> {
        self.write(|tx| {
            let row = tx
                .query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM cron_deliveries WHERE state = 'pending' \
                         AND next_attempt_at <= ?1 ORDER BY next_attempt_at, created_at, id LIMIT 1"
                    ),
                    params![now],
                    from_row,
                )
                .optional()
                .map_err(sql)?;
            let Some(mut row) = row else {
                return Ok(None);
            };
            "sending".clone_into(&mut row.state);
            row.attempts += 1;
            row.updated_at = now_ms();
            tx.execute(
                "UPDATE cron_deliveries SET state = 'sending', attempts = ?2, updated_at = ?3 \
                 WHERE id = ?1",
                params![row.id, row.attempts, row.updated_at],
            )
            .map_err(sql)?;
            Ok(Some(row))
        })
    }

    /// Settles row `id` in `state` (`pending` again with `next_attempt_at`
    /// for a retry).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_settle_delivery(
        &self,
        id: &str,
        state: &str,
        error: Option<&str>,
        next_attempt_at: i64,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE cron_deliveries SET state = ?2, error = ?3, next_attempt_at = ?4, \
                 updated_at = ?5 WHERE id = ?1",
                params![id, state, error, next_attempt_at, now_ms()],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Settles row `id` as `sent`, recording the platform's message id
    /// when the adapter returned one.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_settle_sent(&self, id: &str, message_id: Option<&str>) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE cron_deliveries SET state = 'sent', error = NULL, message_id = ?2, \
                 updated_at = ?3 WHERE id = ?1",
                params![id, message_id, now_ms()],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Marks every row still `sending` as `unknown` (their sender died
    /// mid-send) and returns them.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_orphaned_sends(&self) -> Result<Vec<DeliveryRow>, Error> {
        self.write(|tx| {
            let rows = select(tx, "WHERE state = 'sending' ORDER BY created_at, id", &[])?;
            tx.execute(
                "UPDATE cron_deliveries SET state = 'unknown', error = 'unknown outcome', \
                 updated_at = ?1 WHERE state = 'sending'",
                params![now_ms()],
            )
            .map_err(sql)?;
            Ok(rows)
        })
    }

    /// The rows of run `run_id`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_deliveries_of(&self, run_id: &str) -> Result<Vec<DeliveryRow>, Error> {
        self.read(|conn| {
            select(
                conn,
                "WHERE run_id = ?1 ORDER BY created_at, id",
                &[&run_id],
            )
        })
    }

    /// Every row not yet `sent` or `suppressed`, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_undelivered(&self) -> Result<Vec<DeliveryRow>, Error> {
        self.read(|conn| {
            select(
                conn,
                "WHERE state NOT IN ('sent', 'suppressed') ORDER BY created_at, id",
                &[],
            )
        })
    }
}
