//! Finishing work (spec: kanban, Completion gates; Review lane; Archive
//! terminates the worker; Workspaces). Every refused completion is
//! recorded as a `completion_refused` event before the error returns.

use std::path::PathBuf;

use pmt_core::error::{Error, codes};
use rusqlite::{Transaction, params};
use serde_json::json;

use super::flow::{end_open_run, gate, own_run};
use super::{Board, event, load, open_parents, open_run, set_status};
use crate::attach::{self, Attachment};
use crate::db::sql;
use crate::model::{Actor, ActorKind, Status, Task, TaskKind, Workspace};
use crate::process;
use crate::workflow::{self, Verb};

/// What `complete` reports.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    /// The result.
    pub result: Option<String>,
    /// The summary.
    pub summary: Option<String>,
    /// Tasks the run says it created.
    pub created_tasks: Vec<String>,
    /// The run a person names to complete a task a live worker owns.
    pub run_id: Option<String>,
    /// Complete despite a live worker.
    pub force: bool,
    /// Largest artifact accepted into attachments.
    pub attachment_limit: u64,
    /// `metadata.published_pr` and other facts the completer reports.
    pub metadata: Option<serde_json::Value>,
}

/// What handing a task to review names.
#[derive(Debug, Clone, Default)]
pub struct Handoff {
    /// The summary.
    pub summary: Option<String>,
    /// `metadata.published_pr` binds a repository contract's PR.
    pub metadata: Option<serde_json::Value>,
    /// A reviewer profile to set.
    pub reviewer: Option<String>,
}

/// Declared artifacts read from the workspace: (name, bytes).
type Files = Vec<(String, Vec<u8>)>;

/// A refusal: the message and the event kind it is recorded as (with
/// its payload, when it carries more than the reason).
pub(super) struct Refusal {
    pub(super) kind: &'static str,
    pub(super) message: String,
    pub(super) payload: Option<serde_json::Value>,
}

impl Refusal {
    pub(super) fn gate(message: String) -> Self {
        Self {
            kind: "completion_refused",
            message,
            payload: None,
        }
    }
}

fn blank(text: Option<&str>) -> bool {
    text.is_none_or(|t| t.trim().is_empty())
}

/// The completion gates that need no file system.
fn gates(
    tx: &Transaction<'_>,
    actor: &Actor,
    task: &Task,
    c: &Completion,
) -> Result<Option<Refusal>, Error> {
    let id = &task.id;
    let open = open_parents(tx, id)?;
    if !open.is_empty() {
        return Ok(Some(Refusal::gate(format!(
            "completion blocked: {id} has open parents: {}",
            open.join(", ")
        ))));
    }
    if blank(c.result.as_deref()) && blank(c.summary.as_deref()) {
        return Ok(Some(Refusal::gate(format!(
            "completion blocked: {id} has no result or summary evidence"
        ))));
    }
    let run = actor.run.as_deref().or(c.run_id.as_deref());
    let mut foreign = Vec::new();
    for created in &c.created_tasks {
        let ok = load(tx, created)
            .is_ok_and(|t| t.created_run.is_some() && t.created_run.as_deref() == run);
        if !ok {
            foreign.push(created.clone());
        }
    }
    if !foreign.is_empty() {
        return Ok(Some(Refusal {
            kind: "hallucinated_cards",
            message: format!(
                "completion blocked: created_tasks names task(s) this run did not create: {}",
                foreign.join(", ")
            ),
            payload: None,
        }));
    }
    if actor.kind != ActorKind::Worker
        && let Some(open) = open_run(tx, id)?
        && open
            .pid
            .zip(open.pid_start)
            .is_some_and(|(pid, start)| process::alive(pid, start))
        && c.run_id.as_deref() != Some(open.id.as_str())
        && !c.force
    {
        return Ok(Some(Refusal::gate(format!(
            "task {id} is running under a live worker; pass the run id or --force"
        ))));
    }
    Ok(None)
}

impl Board {
    pub(super) fn refuse(&mut self, actor: &Actor, id: &str, refusal: &Refusal, now: i64) -> Error {
        let run = actor.run.clone();
        let payload = refusal
            .payload
            .clone()
            .unwrap_or_else(|| json!({"reason": refusal.message}));
        let _ = self.record(Some(id), run.as_deref(), actor, refusal.kind, &payload, now);
        Error::new(&codes::KANBAN_COMPLETION_BLOCKED, refusal.message.clone())
    }

    /// Where a completion by `actor` sends `task`.
    fn completion_target(&self, actor: &Actor, task: &Task) -> Status {
        let lane = task.review.unwrap_or(self.settings().review);
        let evolve = matches!(task.kind, TaskKind::Evolve { .. });
        if evolve || (actor.kind == ActorKind::Worker && lane) {
            Status::Review
        } else {
            Status::Done
        }
    }

    /// The workspace directory a task's artifacts are read from.
    #[must_use]
    pub fn workspace_dir(&self, task: &Task) -> PathBuf {
        match (&task.workspace_path, &task.workspace) {
            (Some(path), _) | (None, Workspace::Dir { path }) => path.clone(),
            (None, _) => self.scratch_dir(&task.id),
        }
    }

    /// Reads the declared artifacts; the first missing one refuses.
    fn artifacts(&self, task: &Task, limit: u64) -> Result<Result<Files, Refusal>, Error> {
        let dir = self.workspace_dir(task);
        let mut out = Vec::new();
        for name in &task.artifacts {
            let path = dir.join(name);
            if !path.is_file() {
                return Ok(Err(Refusal::gate(format!(
                    "declared artifact missing: {name}"
                ))));
            }
            out.push((name.clone(), attach::read_limited(&path, limit)?));
        }
        Ok(Ok(out))
    }

    /// Runs the completion gates of `task` moving to `to`; a refusal is
    /// recorded before it returns.
    fn admit(
        &mut self,
        actor: &Actor,
        task: &Task,
        c: &Completion,
        to: Status,
        now: i64,
    ) -> Result<(), Error> {
        let id = &task.id;
        let refusal = {
            let tx = self.conn().unchecked_transaction().map_err(sql)?;
            own_run(&tx, actor, id)?;
            gates(&tx, actor, task, c)?
        };
        let refusal = match refusal {
            Some(refusal) => Some(refusal),
            None => self.goal_gate(task)?,
        };
        let refusal = match (refusal, to) {
            (Some(refusal), _) => Some(refusal),
            (None, Status::Done) => self.contract_gate(actor, task, c.metadata.as_ref(), now)?,
            (None, _) => {
                self.bind_published(actor, task, c.metadata.as_ref(), now)?;
                None
            }
        };
        match refusal {
            Some(refusal) => Err(self.refuse(actor, id, &refusal, now)),
            None => Ok(()),
        }
    }

    /// Completes a task through the completion gates; returns `done`, or
    /// `review` when the review lane takes a worker's completion.
    ///
    /// # Errors
    ///
    /// `kanban.completion_blocked` (recorded), `kanban.illegal_transition`,
    /// `kanban.not_permitted`.
    pub fn complete(
        &mut self,
        actor: &Actor,
        id: &str,
        c: &Completion,
        now: i64,
    ) -> Result<Status, Error> {
        let task = self.task(id)?;
        let to = self.completion_target(actor, &task);
        workflow::check(Verb::Complete, actor.kind, task.status, to)?;
        self.admit(actor, &task, c, to, now)?;
        let files = match self.artifacts(&task, c.attachment_limit)? {
            Ok(files) => files,
            Err(refusal) => return Err(self.refuse(actor, id, &refusal, now)),
        };
        let mut stored = Vec::new();
        for (name, bytes) in files {
            let sha = attach::put_blob(self.blobs(), &bytes)?;
            stored.push(Attachment {
                id: crate::model::prefixed_id("a"),
                task_id: id.to_owned(),
                content_type: attach::guess_type(&name).to_owned(),
                filename: attach::sanitize(&name),
                size: bytes.len() as u64,
                sha256: sha,
                uploader: actor.name.clone(),
                at: now,
            });
        }
        let label = actor.label();
        self.write(|tx| {
            // The status may have moved since the gates were read.
            let current = load(tx, id)?;
            workflow::check(Verb::Complete, actor.kind, current.status, to)?;
            own_run(tx, actor, id)?;
            for a in &stored {
                attach::insert(tx, a)?;
            }
            let outcome = if to == Status::Done { "completed" } else { "review" };
            let run = end_open_run(tx, id, outcome, now)?;
            set_status(tx, id, to, now)?;
            tx.execute(
                "UPDATE tasks SET result = ?2, summary = ?3, consecutive_failures = 0, last_failure = NULL, \
                 block_history = '[]', completed_at = ?4 WHERE id = ?1",
                params![id, c.result, c.summary, (to == Status::Done).then_some(now)],
            )
            .map_err(sql)?;
            let kind = if to == Status::Done { "completed" } else { "review_requested" };
            let summary = c.summary.as_deref().or(c.result.as_deref());
            event(tx, Some(id), run.as_deref(), &label, kind, &json!({"summary": summary}), now)?;
            Ok(())
        })?;
        if to == Status::Done {
            self.drop_scratch(&task);
        }
        Ok(to)
    }

    /// Deletes a done task's scratch workspace (its artifacts are
    /// attachments by now); worktrees and directories are preserved.
    fn drop_scratch(&self, task: &Task) {
        if task.workspace == Workspace::Scratch {
            let _ = std::fs::remove_dir_all(self.scratch_dir(&task.id));
        }
    }

    /// Hands a task to review (`running` or `ready`).
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition`, `kanban.not_permitted`.
    pub fn request_review(
        &mut self,
        actor: &Actor,
        id: &str,
        handoff: &Handoff,
        now: i64,
    ) -> Result<(), Error> {
        let task = self.task(id)?;
        if let Some(refusal) = self.goal_gate(&task)? {
            return Err(self.refuse(actor, id, &refusal, now));
        }
        self.bind_published(actor, &task, handoff.metadata.as_ref(), now)?;
        let summary = handoff.summary.as_deref();
        let reviewer = handoff.reviewer.as_deref();
        let label = actor.label();
        self.write(|tx| {
            let task = load(tx, id)?;
            own_run(tx, actor, id)?;
            workflow::check(Verb::RequestReview, actor.kind, task.status, Status::Review)?;
            let run = end_open_run(tx, id, "review", now)?;
            set_status(tx, id, Status::Review, now)?;
            if let Some(summary) = summary {
                tx.execute(
                    "UPDATE tasks SET summary = ?2 WHERE id = ?1",
                    params![id, summary],
                )
                .map_err(sql)?;
            }
            if let Some(reviewer) = reviewer {
                tx.execute(
                    "UPDATE tasks SET reviewer = ?2 WHERE id = ?1",
                    params![id, reviewer],
                )
                .map_err(sql)?;
            }
            event(
                tx,
                Some(id),
                run.as_deref(),
                &label,
                "review_requested",
                &json!({"summary": summary, "reviewer": reviewer}),
                now,
            )?;
            Ok(())
        })
    }

    /// A reviewer accepts: `review → done`.
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition`, `kanban.completion_blocked` with open
    /// parents.
    pub fn approve(
        &mut self,
        actor: &Actor,
        id: &str,
        comment: Option<&str>,
        now: i64,
    ) -> Result<(), Error> {
        let task = self.task(id)?;
        workflow::check(Verb::Approve, actor.kind, task.status, Status::Done)?;
        // Approving an evolve task adopts a harness: a person's call only
        // (spec: kanban, Eval and harness-evolution tasks).
        if matches!(task.kind, TaskKind::Evolve { .. }) && actor.kind != ActorKind::User {
            return Err(Error::new(
                &codes::KANBAN_NOT_PERMITTED,
                format!(
                    "{id} is an evolve task: only a person approves it (prometheus kanban approve {id})"
                ),
            ));
        }
        if let Some(refusal) = self.contract_gate(actor, &task, None, now)? {
            return Err(self.refuse(actor, id, &refusal, now));
        }
        let label = actor.label();
        self.write(|tx| {
            let current = load(tx, id)?;
            workflow::check(Verb::Approve, actor.kind, current.status, Status::Done)?;
            let open = open_parents(tx, id)?;
            if !open.is_empty() {
                return Err(Error::new(
                    &codes::KANBAN_COMPLETION_BLOCKED,
                    format!("completion blocked: {id} has open parents: {}", open.join(", ")),
                ));
            }
            let run = end_open_run(tx, id, "completed", now)?;
            set_status(tx, id, Status::Done, now)?;
            tx.execute(
                "UPDATE tasks SET completed_at = ?2, consecutive_failures = 0, block_history = '[]' WHERE id = ?1",
                params![id, now],
            )
            .map_err(sql)?;
            comment_in(tx, id, &label, comment, now)?;
            event(tx, Some(id), run.as_deref(), &label, "completed", &json!({"approved": true, "summary": current.summary}), now)?;
            Ok(())
        })?;
        self.drop_scratch(&task);
        Ok(())
    }

    /// A reviewer sends the task back to `todo` with `comment`; the next
    /// promotion pass re-gates it.
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition`, `kanban.invalid_task` without a
    /// comment.
    pub fn request_changes(
        &mut self,
        actor: &Actor,
        id: &str,
        comment: &str,
        now: i64,
    ) -> Result<(), Error> {
        if comment.trim().is_empty() {
            return Err(crate::model::invalid("request-changes needs a comment"));
        }
        let label = actor.label();
        self.write(|tx| {
            let task = load(tx, id)?;
            workflow::check(Verb::RequestChanges, actor.kind, task.status, Status::Todo)?;
            let run = end_open_run(tx, id, "changes_requested", now)?;
            set_status(tx, id, Status::Todo, now)?;
            comment_in(tx, id, &label, Some(comment), now)?;
            event(
                tx,
                Some(id),
                run.as_deref(),
                &label,
                "changes_requested",
                &json!({"comment": comment}),
                now,
            )?;
            Ok(())
        })
    }

    /// Archives a task. When it was running and this caller won the
    /// transition, its worker is terminated after the commit and the
    /// signal's result recorded; its subscriptions are dropped.
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition`, `kanban.task_not_found`.
    pub fn archive(&mut self, actor: &Actor, id: &str, now: i64) -> Result<(), Error> {
        let label = actor.label();
        let worker = self.write(|tx| {
            let task = load(tx, id)?;
            workflow::check(Verb::Archive, actor.kind, task.status, Status::Archived)?;
            let open = open_run(tx, id)?;
            end_open_run(tx, id, "archived", now)?;
            set_status(tx, id, Status::Archived, now)?;
            tx.execute("DELETE FROM subscriptions WHERE task_id = ?1", [id])
                .map_err(sql)?;
            event(
                tx,
                Some(id),
                None,
                &label,
                "archived",
                &json!({"from": task.status}),
                now,
            )?;
            Ok(open.filter(|_| task.status == Status::Running))
        })?;
        if let Some(run) = worker
            && let (Some(pid), Some(start)) = (run.pid, run.pid_start)
        {
            let result = process::terminate(pid, start);
            self.record(
                Some(id),
                Some(&run.id),
                actor,
                "archive_worker_termination",
                &json!({"pid": pid, "signal": result.as_str()}),
                now,
            )?;
        }
        Ok(())
    }

    /// Restores an archived task to `triage`, `todo` or `ready`.
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition`.
    pub fn restore(
        &mut self,
        actor: &Actor,
        id: &str,
        to: Option<Status>,
        now: i64,
    ) -> Result<Status, Error> {
        let label = actor.label();
        self.write(|tx| {
            let task = load(tx, id)?;
            let to = match to {
                Some(to) => to,
                None => gate(tx, id)?,
            };
            workflow::check(Verb::Restore, actor.kind, task.status, to)?;
            if to == Status::Ready && !open_parents(tx, id)?.is_empty() {
                return Err(workflow::illegal(actor.kind, task.status, to));
            }
            set_status(tx, id, to, now)?;
            event(
                tx,
                Some(id),
                None,
                &label,
                "restored",
                &json!({"status": to}),
                now,
            )?;
            Ok(to)
        })
    }
}

fn comment_in(
    tx: &Transaction<'_>,
    id: &str,
    author: &str,
    body: Option<&str>,
    now: i64,
) -> Result<(), Error> {
    if let Some(body) = body.filter(|b| !b.trim().is_empty()) {
        tx.execute(
            "INSERT INTO comments(task_id, author, body, at) VALUES (?1, ?2, ?3, ?4)",
            params![id, author, body, now],
        )
        .map_err(sql)?;
    }
    Ok(())
}
