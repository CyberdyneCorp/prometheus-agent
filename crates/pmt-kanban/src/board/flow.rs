//! Moving work along (spec: kanban, Ready promotion and atomic claim;
//! Typed blocks and block-loop breaker; Scheduled parking).

use pmt_core::error::{Error, codes};
use rusqlite::{Transaction, params};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};

use super::{Board, event, load, open_parents, open_run, set_status};
use crate::db::sql;
use crate::model::{Actor, ActorKind, BlockKind, Status, Task};
use crate::workflow::{self, Verb};

/// Which kind of run a claim opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Works a `ready` task (it becomes `running`).
    Worker,
    /// Reviews a `review` task (it stays in `review`).
    Reviewer,
}

impl Role {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
        }
    }
}

/// A won claim.
#[derive(Debug, Clone)]
pub struct Claim {
    /// The new run.
    pub run_id: String,
    /// The single-use secret handed to the worker on stdin (only its hash
    /// is stored).
    pub secret: String,
    /// The task as claimed.
    pub task: Task,
}

/// The outcome of a claim attempt.
#[derive(Debug, Clone)]
pub enum ClaimOutcome {
    /// The caller owns the task now.
    Claimed(Box<Claim>),
    /// A parent re-opened: the task went back to `todo`.
    Rejected(Vec<String>),
}

/// A fresh run id.
pub(crate) fn run_id() -> String {
    crate::model::prefixed_id("r")
}

/// The SHA-256 of a ticket secret, as stored.
#[must_use]
pub fn secret_hash(secret: &str) -> String {
    crate::model::hex(&Sha256::digest(secret.as_bytes()))
}

fn secret() -> String {
    let mut bytes = [0_u8; 32];
    let _ = getrandom::fill(&mut bytes);
    crate::model::hex(&bytes)
}

/// Ends `task`'s open run with `outcome` inside `tx`.
pub(crate) fn end_open_run(
    tx: &Transaction<'_>,
    task: &str,
    outcome: &str,
    now: i64,
) -> Result<Option<String>, Error> {
    let Some(run) = open_run(tx, task)? else {
        return Ok(None);
    };
    tx.execute(
        "UPDATE task_runs SET ended_at = ?2, outcome = ?3 WHERE id = ?1",
        params![run.id, now, outcome],
    )
    .map_err(sql)?;
    Ok(Some(run.id))
}

/// Checks that a worker actor acts in the task's open run.
pub(crate) fn own_run(tx: &Transaction<'_>, actor: &Actor, task: &str) -> Result<(), Error> {
    if actor.kind != ActorKind::Worker {
        return Ok(());
    }
    let open = open_run(tx, task)?.map(|r| r.id);
    if open.is_some() && open == actor.run {
        Ok(())
    } else {
        Err(Error::new(
            &codes::KANBAN_NOT_PERMITTED,
            format!(
                "run {} does not own task {task}",
                actor.run.as_deref().unwrap_or("-")
            ),
        ))
    }
}

fn history(tx: &Transaction<'_>, id: &str) -> Result<Vec<String>, Error> {
    let text: String = tx
        .query_row("SELECT block_history FROM tasks WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .map_err(sql)?;
    Ok(serde_json::from_str(&text).unwrap_or_default())
}

impl Board {
    /// The promotion pass and scheduled wake-ups: `todo` tasks whose
    /// parents are settled become `ready`; scheduled tasks whose
    /// `wake_at` passed are re-gated. Returns the promoted ids.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn promote_all(&mut self, actor: &Actor, now: i64) -> Result<Vec<String>, Error> {
        let label = actor.label();
        self.write(|tx| {
            let mut moved = Vec::new();
            let todo = ids(tx, "SELECT id FROM tasks WHERE status = 'todo' ORDER BY created_at", [])?;
            for id in todo {
                if open_parents(tx, &id)?.is_empty() {
                    workflow::check(Verb::Promote, actor.kind, Status::Todo, Status::Ready)?;
                    set_status(tx, &id, Status::Ready, now)?;
                    event(tx, Some(&id), None, &label, "promoted", &json!({}), now)?;
                    moved.push(id);
                }
            }
            let due = ids(
                tx,
                "SELECT id FROM tasks WHERE status = 'scheduled' AND wake_at IS NOT NULL AND wake_at <= ?1",
                [now],
            )?;
            for id in due {
                let to = gate(tx, &id)?;
                workflow::check(Verb::Wake, actor.kind, Status::Scheduled, to)?;
                set_status(tx, &id, to, now)?;
                tx.execute("UPDATE tasks SET wake_at = NULL WHERE id = ?1", [&id])
                    .map_err(sql)?;
                event(tx, Some(&id), None, &label, "woke", &json!({"status": to}), now)?;
                if to == Status::Ready {
                    moved.push(id);
                }
            }
            Ok(moved)
        })
    }

    /// Claims `id` for `profile` atomically: `ready → running` with a new
    /// run (or, for a reviewer, a run on a `review` task), leased for
    /// `ttl_ms`. A parent that re-opened demotes the task instead.
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition` when the task is not claimable (another
    /// claimer won), `kanban.task_not_found`.
    pub fn claim(
        &mut self,
        actor: &Actor,
        id: &str,
        profile: &str,
        role: Role,
        ttl_ms: i64,
        now: i64,
    ) -> Result<ClaimOutcome, Error> {
        let label = actor.label();
        self.write(|tx| {
            let task = load(tx, id)?;
            match role {
                Role::Worker => {
                    workflow::check(Verb::Claim, actor.kind, task.status, Status::Running)?;
                    let open = open_parents(tx, id)?;
                    if !open.is_empty() {
                        set_status(tx, id, Status::Todo, now)?;
                        event(tx, Some(id), None, &label, "claim_rejected", &json!({"open_parents": open}), now)?;
                        return Ok(ClaimOutcome::Rejected(open));
                    }
                    set_status(tx, id, Status::Running, now)?;
                }
                Role::Reviewer => {
                    if task.status != Status::Review || open_run(tx, id)?.is_some() {
                        return Err(workflow::illegal(actor.kind, task.status, Status::Review));
                    }
                }
            }
            let run = run_id();
            let secret = secret();
            tx.execute(
                "INSERT INTO task_runs(id, task_id, profile, role, started_at, lease_expires_at, ticket_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![run, id, profile, role.as_str(), now, now + ttl_ms, secret_hash(&secret)],
            )
            .map_err(sql)?;
            tx.execute("UPDATE tasks SET not_before = NULL WHERE id = ?1", [id])
                .map_err(sql)?;
            event(tx, Some(id), Some(&run), &label, "claimed", &json!({"profile": profile, "role": role}), now)?;
            Ok(ClaimOutcome::Claimed(Box::new(Claim {
                run_id: run,
                secret,
                task: load(tx, id)?,
            })))
        })
    }

    /// Blocks a task with a typed reason; returns where it went.
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition`, `kanban.not_permitted` (a worker
    /// outside its run), `kanban.invalid_task` for a blank reason.
    pub fn block(
        &mut self,
        actor: &Actor,
        id: &str,
        kind: BlockKind,
        reason: &str,
        now: i64,
    ) -> Result<Status, Error> {
        if reason.trim().is_empty() {
            return Err(crate::model::invalid("a block needs a reason"));
        }
        let label = actor.label();
        self.write(|tx| {
            let task = load(tx, id)?;
            own_run(tx, actor, id)?;
            let open = open_parents(tx, id)?;
            let kind = if kind == BlockKind::Dependency && open.is_empty() {
                BlockKind::NeedsInput
            } else {
                kind
            };
            let looped = history(tx, id)?.iter().any(|k| k == kind.as_str());
            let (verb, to) = if kind == BlockKind::Dependency {
                (Verb::BlockDependency, Status::Todo)
            } else if looped {
                (Verb::BlockLoop, Status::Triage)
            } else {
                (Verb::Block, Status::Blocked)
            };
            workflow::check(verb, actor.kind, task.status, to)?;
            let run = end_open_run(tx, id, "blocked", now)?;
            set_status(tx, id, to, now)?;
            tx.execute(
                "UPDATE tasks SET block_kind = ?2, block_reason = ?3, blocked_from = ?4 WHERE id = ?1",
                params![
                    id,
                    (to == Status::Blocked).then_some(kind.as_str()),
                    (to == Status::Blocked).then_some(reason.trim()),
                    task.status.as_str()
                ],
            )
            .map_err(sql)?;
            let payload = json!({"kind": kind, "reason": reason.trim(), "status": to});
            event(tx, Some(id), run.as_deref(), &label, "blocked", &payload, now)?;
            if looped {
                event(tx, Some(id), run.as_deref(), &label, "block_loop_detected", &payload, now)?;
            }
            Ok(to)
        })
    }

    /// Lifts a block (or a schedule): back to `review` when it was blocked
    /// out of review, else `ready` or `todo` by its parents. A deliberate
    /// unblock also resets the failure breaker.
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition`.
    pub fn unblock(&mut self, actor: &Actor, id: &str, now: i64) -> Result<Status, Error> {
        let label = actor.label();
        self.write(|tx| {
            let task = load(tx, id)?;
            let from: Option<String> = tx
                .query_row("SELECT blocked_from FROM tasks WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .map_err(sql)?;
            let to = if task.status == Status::Blocked && from.as_deref() == Some("review") {
                Status::Review
            } else {
                gate(tx, id)?
            };
            workflow::check(Verb::Unblock, actor.kind, task.status, to)?;
            let mut past = history(tx, id)?;
            if let Some(kind) = task.block_kind {
                past.push(kind.as_str().to_owned());
            }
            set_status(tx, id, to, now)?;
            tx.execute(
                "UPDATE tasks SET block_kind = NULL, block_reason = NULL, blocked_from = NULL, \
                 wake_at = NULL, breaker_tripped = 0, consecutive_failures = 0, block_history = ?2 \
                 WHERE id = ?1",
                params![id, serde_json::to_string(&past).unwrap_or_default()],
            )
            .map_err(sql)?;
            event(
                tx,
                Some(id),
                None,
                &label,
                "unblocked",
                &json!({"status": to}),
                now,
            )?;
            Ok(to)
        })
    }

    /// Parks a task until `wake_at` (Unix ms; `None`: until unblocked),
    /// ending any open run with outcome `scheduled`.
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition`, `kanban.not_permitted`.
    pub fn schedule(
        &mut self,
        actor: &Actor,
        id: &str,
        wake_at: Option<i64>,
        now: i64,
    ) -> Result<(), Error> {
        let label = actor.label();
        self.write(|tx| {
            let task = load(tx, id)?;
            own_run(tx, actor, id)?;
            workflow::check(Verb::Schedule, actor.kind, task.status, Status::Scheduled)?;
            let run = end_open_run(tx, id, "scheduled", now)?;
            set_status(tx, id, Status::Scheduled, now)?;
            tx.execute(
                "UPDATE tasks SET wake_at = ?2, block_kind = NULL, block_reason = NULL WHERE id = ?1",
                params![id, wake_at],
            )
            .map_err(sql)?;
            event(tx, Some(id), run.as_deref(), &label, "scheduled", &json!({"wake_at": wake_at}), now)?;
            Ok(())
        })
    }

    /// Moves a task as a person's drag would, through the workflow table.
    /// Only the plain moves are made here (planning, re-queue, restore);
    /// moves that need an operation's inputs (complete, archive, block)
    /// name it.
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition` naming the actor.
    pub fn move_to(&mut self, actor: &Actor, id: &str, to: Status, now: i64) -> Result<(), Error> {
        let label = actor.label();
        self.write(|tx| {
            let task = load(tx, id)?;
            let verb = workflow::move_verb(actor.kind, task.status, to)?;
            let plain = matches!(
                verb,
                Verb::Plan | Verb::Requeue | Verb::Restore | Verb::Promote
            );
            if !plain {
                return Err(Error::new(
                    &codes::KANBAN_ILLEGAL_TRANSITION,
                    format!(
                        "moving {} → {to} is `{}`: use that operation",
                        task.status,
                        verb.as_str()
                    ),
                ));
            }
            if to == Status::Ready && !open_parents(tx, id)?.is_empty() {
                return Err(workflow::illegal(actor.kind, task.status, to));
            }
            set_status(tx, id, to, now)?;
            event(
                tx,
                Some(id),
                None,
                &label,
                "moved",
                &json!({"from": task.status, "to": to, "verb": verb.as_str()}),
                now,
            )?;
            Ok(())
        })
    }
}

/// `ready` when every parent is settled, else `todo`.
pub(crate) fn gate(tx: &Transaction<'_>, id: &str) -> Result<Status, Error> {
    Ok(if open_parents(tx, id)?.is_empty() {
        Status::Ready
    } else {
        Status::Todo
    })
}

fn ids<P: rusqlite::Params>(
    tx: &Transaction<'_>,
    query: &str,
    params: P,
) -> Result<Vec<String>, Error> {
    let mut stmt = tx.prepare(query).map_err(sql)?;
    let rows = stmt
        .query_map(params, |r| r.get(0))
        .map_err(sql)?
        .collect::<Result<Vec<String>, _>>()
        .map_err(sql)?;
    Ok(rows)
}
