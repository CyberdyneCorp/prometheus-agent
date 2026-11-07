//! Durable delegation records (spec: subagent-delegation, Durable
//! background completions).
//!
//! A background `delegate_task` call outlives the turn that made it and
//! possibly the process: its row and its children's rows are written as
//! they change, so after a restart the children still marked running are
//! finalised as `interrupted` and the call's completion is delivered to
//! its owning conversation exactly once. Delivery is tracked on the
//! delegation row (`pending` -> `delivered` | `dropped`).

use std::time::Duration;

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension as _, Row, params};

use crate::error::sql;
use crate::sessions::now_ms;
use crate::store::Store;

/// Delivery attempts after which a completion is dropped.
pub const MAX_DELIVERY_ATTEMPTS: i64 = 8;
/// Age after which an undelivered completion is dropped.
pub const DELIVERY_TTL: Duration = Duration::from_hours(48);

/// How a delegation's completion reaches its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// A synchronous call: the result went back inline.
    Inline,
    /// A background call whose completion is still owed.
    Pending,
    /// Delivered once.
    Delivered,
    /// Given up (too old or too many attempts).
    Dropped,
}

impl Delivery {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Dropped => "dropped",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "inline" => Self::Inline,
            "delivered" => Self::Delivered,
            "dropped" => Self::Dropped,
            _ => Self::Pending,
        }
    }
}

/// A delegation row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationRow {
    /// `dg-<ULID>`.
    pub id: String,
    /// The session that made the call.
    pub session_id: String,
    /// Its root lineage.
    pub root_session_id: String,
    /// Creation, Unix milliseconds.
    pub created_at: i64,
    /// Delivery state.
    pub delivery: Delivery,
    /// Delivery attempts so far.
    pub attempts: i64,
    /// When the last child ended.
    pub completed_at: Option<i64>,
    /// What heads its completion (`Review: <focus>`), when the dispatch
    /// named one.
    pub label: Option<String>,
}

/// A child row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRow {
    /// `sa-<ULID>`.
    pub id: String,
    /// Its delegation.
    pub delegation_id: String,
    /// Its task's index in the call.
    pub task_index: i64,
    /// The child's own session, once built.
    pub session_id: Option<String>,
    /// The session that spawned it.
    pub parent_session_id: String,
    /// The root lineage.
    pub root_session_id: String,
    /// Depth from 1.
    pub depth: i64,
    /// The delegated goal.
    pub goal: String,
    /// `queued`, `running` or a terminal status.
    pub status: String,
    /// The `SubagentResult` JSON once terminal.
    pub result: Option<String>,
}

/// Statuses a child cannot leave.
pub const TERMINAL: &[&str] = &["completed", "failed", "interrupted", "stalled", "refused"];

const SUBAGENT_COLUMNS: &str = "id, delegation_id, task_index, session_id, parent_session_id, \
     root_session_id, depth, goal, status, result";

fn subagent_from_row(row: &Row<'_>) -> rusqlite::Result<SubagentRow> {
    Ok(SubagentRow {
        id: row.get(0)?,
        delegation_id: row.get(1)?,
        task_index: row.get(2)?,
        session_id: row.get(3)?,
        parent_session_id: row.get(4)?,
        root_session_id: row.get(5)?,
        depth: row.get(6)?,
        goal: row.get(7)?,
        status: row.get(8)?,
        result: row.get(9)?,
    })
}

fn delegation_from_row(row: &Row<'_>) -> rusqlite::Result<DelegationRow> {
    Ok(DelegationRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        root_session_id: row.get(2)?,
        created_at: row.get(3)?,
        delivery: Delivery::parse(&row.get::<_, String>(4)?),
        attempts: row.get(5)?,
        completed_at: row.get(6)?,
        label: row.get(7)?,
    })
}

const DELEGATION_COLUMNS: &str =
    "id, session_id, root_session_id, created_at, delivery, attempts, completed_at, label";

impl Store {
    /// Records a spawn call of `session` and its children (status
    /// `queued`), in one transaction.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn record_delegation(
        &self,
        id: &str,
        session: SessionId,
        delivery: Delivery,
        children: &[SubagentRow],
    ) -> Result<(), Error> {
        let root = self.session(session)?.root_session_id.to_string();
        let now = now_ms();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO delegations (id, session_id, root_session_id, created_at, delivery) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, session.to_string(), root, now, delivery.as_str()],
            )
            .map_err(sql)?;
            for child in children {
                tx.execute(
                    "INSERT INTO subagents (id, delegation_id, task_index, session_id, \
                     parent_session_id, root_session_id, depth, goal, status, result, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        child.id,
                        id,
                        child.task_index,
                        child.session_id,
                        child.parent_session_id,
                        root,
                        child.depth,
                        child.goal,
                        child.status,
                        child.result,
                        now
                    ],
                )
                .map_err(sql)?;
            }
            Ok(())
        })
    }

    /// Moves child `id` to `status` (`running`), noting its session.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn subagent_started(&self, id: &str, session: SessionId) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE subagents SET status = 'running', session_id = ?2 WHERE id = ?1 \
                 AND status IN ('queued', 'running')",
                params![id, session.to_string()],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Finalises child `id` with `status` and its result JSON (a terminal
    /// row is never rewritten), and stamps its delegation complete when it
    /// was the last child running. Returns whether the delegation is now
    /// complete.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn subagent_finished(&self, id: &str, status: &str, result: &str) -> Result<bool, Error> {
        let now = now_ms();
        self.write(|tx| {
            tx.execute(
                "UPDATE subagents SET status = ?2, result = ?3, finished_at = ?4 WHERE id = ?1 \
                 AND status IN ('queued', 'running')",
                params![id, status, result, now],
            )
            .map_err(sql)?;
            let delegation: Option<String> = tx
                .query_row(
                    "SELECT delegation_id FROM subagents WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql)?;
            let Some(delegation) = delegation else {
                return Ok(false);
            };
            complete_if_done(tx, &delegation, now)
        })
    }

    /// The children of delegation `id`, in task order.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn subagents_of(&self, id: &str) -> Result<Vec<SubagentRow>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(&format!(
                    "SELECT {SUBAGENT_COLUMNS} FROM subagents WHERE delegation_id = ?1 \
                     ORDER BY task_index"
                ))
                .map_err(sql)?;
            stmt.query_map([id], subagent_from_row)
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)
        })
    }

    /// Delegation `id`, if recorded.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn delegation(&self, id: &str) -> Result<Option<DelegationRow>, Error> {
        self.read(|conn| {
            conn.query_row(
                &format!("SELECT {DELEGATION_COLUMNS} FROM delegations WHERE id = ?1"),
                [id],
                delegation_from_row,
            )
            .optional()
            .map_err(sql)
        })
    }

    /// Names what heads delegation `id`'s completion (`/review`'s
    /// `Review: <focus>`), so a completion delivered after a restart keeps
    /// it.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn set_delegation_label(&self, id: &str, label: &str) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE delegations SET label = ?2 WHERE id = ?1",
                params![id, label],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Children left `queued` or `running` by a process that stopped
    /// (call it once at startup, before any new spawn): each becomes
    /// `interrupted` with the result `interrupted(row)` builds, and their
    /// delegations complete. Returns the rows changed.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn interrupt_unfinished(
        &self,
        interrupted: &dyn Fn(&SubagentRow) -> String,
    ) -> Result<Vec<SubagentRow>, Error> {
        let rows: Vec<SubagentRow> = self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(&format!(
                    "SELECT {SUBAGENT_COLUMNS} FROM subagents WHERE status IN ('queued', 'running')"
                ))
                .map_err(sql)?;
            stmt.query_map([], subagent_from_row)
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)
        })?;
        for row in &rows {
            self.subagent_finished(&row.id, "interrupted", &interrupted(row))?;
        }
        Ok(rows)
    }

    /// Completed background delegations whose completion is still owed to
    /// `root`'s conversation, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn pending_completions(&self, root: SessionId) -> Result<Vec<DelegationRow>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(&format!(
                    "SELECT {DELEGATION_COLUMNS} FROM delegations WHERE delivery = 'pending' \
                     AND completed_at IS NOT NULL AND root_session_id = ?1 ORDER BY created_at"
                ))
                .map_err(sql)?;
            stmt.query_map([root.to_string()], delegation_from_row)
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)
        })
    }

    /// Claims delegation `id`'s completion for delivery: `true` exactly
    /// once (the row moves to `delivered`), `false` when it was already
    /// delivered or dropped.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn claim_completion(&self, id: &str) -> Result<bool, Error> {
        let now = now_ms();
        self.write(|tx| {
            tx.execute(
                "UPDATE delegations SET delivery = 'delivered', delivered_at = ?2, \
                 attempts = attempts + 1 WHERE id = ?1 AND delivery = 'pending'",
                params![id, now],
            )
            .map(|changed| changed == 1)
            .map_err(sql)
        })
    }

    /// Puts a claimed completion back after a failed delivery. Returns
    /// `false` when that was the last allowed attempt or the completion
    /// is older than [`DELIVERY_TTL`]: the row is `dropped` instead.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn release_completion(&self, id: &str) -> Result<bool, Error> {
        let oldest = now_ms() - i64::try_from(DELIVERY_TTL.as_millis()).unwrap_or(i64::MAX);
        self.write(|tx| {
            let retried = tx
                .execute(
                    "UPDATE delegations SET delivery = 'pending', delivered_at = NULL WHERE id = ?1 \
                     AND delivery = 'delivered' AND attempts < ?2 AND created_at >= ?3",
                    params![id, MAX_DELIVERY_ATTEMPTS, oldest],
                )
                .map_err(sql)?;
            if retried == 0 {
                tx.execute(
                    "UPDATE delegations SET delivery = 'dropped' WHERE id = ?1",
                    [id],
                )
                .map_err(sql)?;
            }
            Ok(retried == 1)
        })
    }

    /// Drops pending completions older than [`DELIVERY_TTL`]; returns
    /// their ids (the caller WARNs).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn drop_stale_completions(&self) -> Result<Vec<String>, Error> {
        let oldest = now_ms() - i64::try_from(DELIVERY_TTL.as_millis()).unwrap_or(i64::MAX);
        self.write(|tx| {
            let mut stmt = tx
                .prepare_cached(
                    "UPDATE delegations SET delivery = 'dropped' WHERE delivery = 'pending' \
                     AND created_at < ?1 RETURNING id",
                )
                .map_err(sql)?;
            stmt.query_map([oldest], |row| row.get(0))
                .map_err(sql)?
                .collect::<Result<Vec<String>, _>>()
                .map_err(sql)
        })
    }

    /// Backdates delegation `id` by `age` (tests of the 48-hour rule).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    #[doc(hidden)]
    pub fn age_delegation(&self, id: &str, age: Duration) -> Result<(), Error> {
        let at = now_ms() - i64::try_from(age.as_millis()).unwrap_or(i64::MAX);
        self.write(|tx| {
            tx.execute(
                "UPDATE delegations SET created_at = ?2 WHERE id = ?1",
                params![id, at],
            )
            .map(drop)
            .map_err(sql)
        })
    }
}

/// Stamps `delegation` complete when none of its children is still
/// queued or running.
fn complete_if_done(
    tx: &rusqlite::Transaction<'_>,
    delegation: &str,
    now: i64,
) -> Result<bool, Error> {
    let open: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM subagents WHERE delegation_id = ?1 \
             AND status IN ('queued', 'running')",
            [delegation],
            |row| row.get(0),
        )
        .map_err(sql)?;
    if open > 0 {
        return Ok(false);
    }
    tx.execute(
        "UPDATE delegations SET completed_at = COALESCE(completed_at, ?2) WHERE id = ?1",
        params![delegation, now],
    )
    .map_err(sql)?;
    Ok(true)
}
