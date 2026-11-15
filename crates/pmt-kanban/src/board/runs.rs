//! Runs (spec: kanban, Failure circuit breaker; Worker spawn by run
//! ticket; Worker liveness and heartbeats; Event log and run history):
//! single-use tickets, heartbeats renewing the claim lease, worker
//! process identity, spend, and how a run that did not finish with a
//! terminator ends.

use pmt_core::error::{Error, codes};
use rusqlite::{OptionalExtension as _, params};
use serde_json::json;

use super::flow::secret_hash;
use super::{Board, event, load, set_status};
use crate::db::sql;
use crate::model::{Actor, Run, Status, now_ms};

/// The longest stored failure text.
pub const FAILURE_CHARS: usize = 500;

/// Worker exit code: rate limited, requeue without counting.
pub const EXIT_RATE_LIMITED: i32 = 75;
/// Worker exit code: credential revoked, model gone, capability denied.
pub const EXIT_TERMINAL: i32 = 78;

/// How a run ended without a terminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnd {
    /// `crashed`, `failed`, `timed_out`, `spawn_failed`, `rate_limited`,
    /// `reclaimed`.
    pub outcome: &'static str,
    /// The worker's exit code, when known.
    pub exit_code: Option<i32>,
    /// What happened.
    pub message: String,
    /// Whether it counts towards the breaker.
    pub counts: bool,
}

impl RunEnd {
    /// The end of a worker that exited with `code` leaving its run open.
    #[must_use]
    pub fn exited(code: i32, message: Option<String>) -> Self {
        match code {
            EXIT_RATE_LIMITED => Self {
                outcome: "rate_limited",
                exit_code: Some(code),
                message: message.unwrap_or_else(|| "worker was rate limited (exit 75)".to_owned()),
                counts: false,
            },
            _ => Self {
                outcome: "failed",
                exit_code: Some(code),
                message: message.unwrap_or_else(|| {
                    if code == 0 {
                        "worker exited without kanban_complete, kanban_block or kanban_request_review".to_owned()
                    } else {
                        format!("worker exited with code {code}")
                    }
                }),
                counts: true,
            },
        }
    }
}

/// Breaker settings of the dispatcher's profile.
#[derive(Debug, Clone, Copy)]
pub struct Breaker {
    /// `kanban.failure_limit`.
    pub failure_limit: u32,
    /// `kanban.rate_limit_cooldown_secs` in ms.
    pub cooldown_ms: i64,
}

fn truncate(text: &str) -> String {
    text.chars().take(FAILURE_CHARS).collect()
}

impl Board {
    /// Records the worker process of `run`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn spawned(&mut self, run: &str, pid: u32, start: u64, now: i64) -> Result<(), Error> {
        let actor = Actor::dispatcher().label();
        self.write(|tx| {
            tx.execute(
                "UPDATE task_runs SET pid = ?2, pid_start = ?3, heartbeat_at = ?4 WHERE id = ?1",
                params![run, pid, i64::try_from(start).unwrap_or(0), now],
            )
            .map_err(sql)?;
            let task: String = tx
                .query_row("SELECT task_id FROM task_runs WHERE id = ?1", [run], |r| {
                    r.get(0)
                })
                .map_err(sql)?;
            event(
                tx,
                Some(&task),
                Some(run),
                &actor,
                "worker_spawned",
                &json!({"pid": pid}),
                now,
            )?;
            Ok(())
        })
    }

    /// Consumes the single-use ticket of `run`.
    ///
    /// # Errors
    ///
    /// `kanban.ticket_refused`: `run ticket already used`, or no open run
    /// matches.
    pub fn use_ticket(&mut self, run: &str, secret: &str) -> Result<Run, Error> {
        let hash = secret_hash(secret);
        self.write(|tx| {
            let row: Option<(Option<String>, i64, Option<i64>)> = tx
                .query_row(
                    "SELECT ticket_hash, ticket_used, ended_at FROM task_runs WHERE id = ?1",
                    [run],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(sql)?;
            let refused = |m: &str| Error::new(&codes::KANBAN_TICKET_REFUSED, m.to_owned());
            match row {
                Some((Some(stored), used, ended)) if stored == hash => {
                    if used != 0 {
                        return Err(refused("run ticket already used"));
                    }
                    if ended.is_some() {
                        return Err(refused("run ticket does not match an open run"));
                    }
                }
                _ => return Err(refused("run ticket does not match an open run")),
            }
            tx.execute("UPDATE task_runs SET ticket_used = 1 WHERE id = ?1", [run])
                .map_err(sql)?;
            tx.query_row(
                &format!("SELECT {} FROM task_runs WHERE id = ?1", super::RUN_COLUMNS),
                [run],
                super::run_row,
            )
            .map_err(sql)
        })
    }

    /// Renews the claim lease of an open run.
    ///
    /// # Errors
    ///
    /// `kanban.not_permitted` when the run is closed.
    pub fn heartbeat(
        &mut self,
        actor: &Actor,
        run: &str,
        note: Option<&str>,
        ttl_ms: i64,
        now: i64,
    ) -> Result<(), Error> {
        let label = actor.label();
        self.write(|tx| {
            let renewed = tx
                .execute(
                    "UPDATE task_runs SET heartbeat_at = ?2, lease_expires_at = ?3 WHERE id = ?1 AND ended_at IS NULL",
                    params![run, now, now + ttl_ms],
                )
                .map_err(sql)?;
            if renewed == 0 {
                return Err(Error::new(&codes::KANBAN_NOT_PERMITTED, format!("run {run} is not open")));
            }
            if let Some(note) = note.filter(|n| !n.trim().is_empty()) {
                let task: String = tx
                    .query_row("SELECT task_id FROM task_runs WHERE id = ?1", [run], |r| r.get(0))
                    .map_err(sql)?;
                event(tx, Some(&task), Some(run), &label, "heartbeat", &json!({"note": note}), now)?;
            }
            Ok(())
        })
    }

    /// Records the worker's exit code before it exits, so a dispatcher
    /// that did not spawn it still reaps it right.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn worker_exit(
        &mut self,
        run: &str,
        code: i32,
        message: Option<&str>,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE task_runs SET worker_exit = ?2, error = COALESCE(?3, error) WHERE id = ?1",
                params![run, code, message.map(truncate)],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// The exit code a worker recorded for `run`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn recorded_exit(&self, run: &str) -> Result<Option<(i32, Option<String>)>, Error> {
        self.conn()
            .query_row(
                "SELECT worker_exit, error FROM task_runs WHERE id = ?1 AND worker_exit IS NOT NULL",
                [run],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql)
    }

    /// Records a run's session and spend so far.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn record_usage(
        &mut self,
        run: &str,
        session: Option<&str>,
        cost_usd: f64,
        tokens: u64,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE task_runs SET session_id = COALESCE(?2, session_id), trajectory_id = COALESCE(?2, trajectory_id), \
                 cost_usd = ?3, tokens = ?4 WHERE id = ?1",
                params![run, session, cost_usd, i64::try_from(tokens).unwrap_or(i64::MAX)],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Marks a closed run's worker as gone (`signaled`: the dispatcher
    /// terminated it) and records one `worker_exited` event with how it
    /// ended: `clean_exit`, `rate_limited`, `nonzero_exit`, `signaled` or
    /// `unknown`, its exit code and whether the task goes back to work.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn worker_gone(&mut self, run: &str, signaled: bool) -> Result<(), Error> {
        let actor = Actor::dispatcher().label();
        self.write(|tx| {
            let changed = tx
                .execute(
                    "UPDATE task_runs SET worker_gone = 1 WHERE id = ?1 AND worker_gone = 0",
                    [run],
                )
                .map_err(sql)?;
            if changed == 0 {
                return Ok(());
            }
            let (task, pid, code): (String, Option<u32>, Option<i32>) = tx
                .query_row(
                    "SELECT task_id, pid, worker_exit FROM task_runs WHERE id = ?1",
                    [run],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(sql)?;
            let exit_kind = match (code, signaled) {
                (_, true) => "signaled",
                (Some(0), _) => "clean_exit",
                (Some(EXIT_RATE_LIMITED), _) => "rate_limited",
                (Some(_), _) => "nonzero_exit",
                (None, false) => "unknown",
            };
            let status = load(tx, &task)?.status;
            let payload = json!({"pid": pid, "exit_kind": exit_kind, "exit_code": code, "retry": status == Status::Ready, "status": status});
            event(tx, Some(&task), Some(run), &actor, "worker_exited", &payload, now_ms())?;
            Ok(())
        })
    }

    /// Closed runs whose worker has not been seen gone yet.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn lingering(&self) -> Result<Vec<Run>, Error> {
        let mut stmt = self
            .conn()
            .prepare(&format!(
                "SELECT {} FROM task_runs WHERE ended_at IS NOT NULL AND worker_gone = 0 AND pid IS NOT NULL",
                super::RUN_COLUMNS
            ))
            .map_err(sql)?;
        let rows = stmt
            .query_map([], super::run_row)
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(rows)
    }

    /// Ends an open run that finished without a terminator and moves its
    /// task: back to `ready` (rate limited with a cooldown, or a failure
    /// below the limit), or `blocked` with `gave_up` when the breaker
    /// trips. Returns the task's new status (`None` when the run was
    /// already closed).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn end_run(
        &mut self,
        run: &str,
        end: &RunEnd,
        breaker: Breaker,
        now: i64,
    ) -> Result<Option<Status>, Error> {
        let actor = Actor::dispatcher().label();
        self.write(|tx| {
            let row = tx
                .query_row(
                    &format!("SELECT {} FROM task_runs WHERE id = ?1", super::RUN_COLUMNS),
                    [run],
                    super::run_row,
                )
                .optional()
                .map_err(sql)?;
            let Some(row) = row.filter(Run::is_open) else {
                return Ok(None);
            };
            tx.execute(
                "UPDATE task_runs SET ended_at = ?2, outcome = ?3, exit_code = ?4, error = ?5 WHERE id = ?1",
                params![run, now, end.outcome, end.exit_code, truncate(&end.message)],
            )
            .map_err(sql)?;
            let task = load(tx, &row.task_id)?;
            let payload = json!({"outcome": end.outcome, "exit_code": end.exit_code, "message": truncate(&end.message)});
            if row.role != "worker" || task.status != Status::Running {
                event(tx, Some(&task.id), Some(run), &actor, "run_ended", &payload, now)?;
                return Ok(Some(task.status));
            }
            if !end.counts {
                let not_before = (end.outcome == "rate_limited").then_some(now + breaker.cooldown_ms);
                set_status(tx, &task.id, Status::Ready, now)?;
                tx.execute("UPDATE tasks SET not_before = ?2 WHERE id = ?1", params![task.id, not_before])
                    .map_err(sql)?;
                event(tx, Some(&task.id), Some(run), &actor, end.outcome, &payload, now)?;
                return Ok(Some(Status::Ready));
            }
            let failures = task.consecutive_failures + 1;
            let limit = task.max_retries.unwrap_or(breaker.failure_limit).max(1);
            let terminal = end.exit_code == Some(EXIT_TERMINAL);
            tx.execute(
                "UPDATE tasks SET consecutive_failures = ?2, last_failure = ?3 WHERE id = ?1",
                params![task.id, failures, truncate(&end.message)],
            )
            .map_err(sql)?;
            event(tx, Some(&task.id), Some(run), &actor, end.outcome, &payload, now)?;
            if terminal || failures >= limit {
                set_status(tx, &task.id, Status::Blocked, now)?;
                let kind = if terminal { "capability" } else { "transient" };
                tx.execute(
                    "UPDATE tasks SET breaker_tripped = 1, block_kind = ?2, block_reason = ?3, blocked_from = 'running' WHERE id = ?1",
                    params![task.id, kind, truncate(&end.message)],
                )
                .map_err(sql)?;
                event(tx, Some(&task.id), Some(run), &actor, "gave_up", &json!({"failures": failures, "reason": truncate(&end.message)}), now)?;
                return Ok(Some(Status::Blocked));
            }
            set_status(tx, &task.id, Status::Ready, now)?;
            event(tx, Some(&task.id), Some(run), &actor, "reclaimed", &json!({"failures": failures}), now)?;
            Ok(Some(Status::Ready))
        })
    }

    /// Blocks a `ready` task the dispatcher cannot run (its assignee is
    /// gone): kind `capability`.
    ///
    /// # Errors
    ///
    /// Store errors, `kanban.illegal_transition`.
    pub fn give_up(
        &mut self,
        id: &str,
        kind: crate::model::BlockKind,
        reason: &str,
        now: i64,
    ) -> Result<(), Error> {
        let actor = Actor::dispatcher();
        let label = actor.label();
        self.write(|tx| {
            let task = load(tx, id)?;
            crate::workflow::check(crate::workflow::Verb::GiveUp, actor.kind, task.status, Status::Blocked)?;
            set_status(tx, id, Status::Blocked, now)?;
            tx.execute(
                "UPDATE tasks SET block_kind = ?2, block_reason = ?3, blocked_from = ?4 WHERE id = ?1",
                params![id, kind.as_str(), reason, task.status.as_str()],
            )
            .map_err(sql)?;
            event(tx, Some(id), None, &label, "blocked", &json!({"kind": kind, "reason": reason}), now)?;
            Ok(())
        })
    }

    /// Records where a claimed task's workspace resolved.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn set_workspace_path(&mut self, id: &str, path: &std::path::Path) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE tasks SET workspace_path = ?2 WHERE id = ?1",
                params![id, path.to_string_lossy()],
            )
            .map_err(sql)?;
            Ok(())
        })
    }
}
