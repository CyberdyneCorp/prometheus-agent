//! Task management verbs of the upgrade CLI (spec: kanban, Additional CLI
//! verbs; Per-task model override and pinned skills; Orchestrator
//! guidance, specify and decompose; Worker lanes): model overrides,
//! reopening review, specifying a triage idea, and run-ticket
//! authentication for external workers.

use pmt_core::error::{Error, codes};
use rusqlite::{OptionalExtension as _, params};
use serde_json::json;

use super::create::{NewTask, Profiles};
use super::flow::{end_open_run, gate, secret_hash};
use super::{Board, event, load, set_status};
use crate::db::sql;
use crate::model::{Actor, Run, Status, invalid};
use crate::workflow::{self, Verb};

impl Board {
    /// Sets (or with `None` clears) a task's model override; it applies
    /// from the next dispatch.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task` (`--provider` without a model, a model the
    /// registry refuses), `kanban.task_not_found`.
    pub fn set_model(
        &mut self,
        actor: &Actor,
        id: &str,
        model: Option<&str>,
        provider: Option<&str>,
        profiles: &dyn Profiles,
        now: i64,
    ) -> Result<(), Error> {
        let task = self.task(id)?;
        if provider.is_some() && model.is_none() {
            return Err(invalid("--provider requires a model"));
        }
        if model.is_some() {
            let mut probe = NewTask::titled(&task.title);
            probe.assignee.clone_from(&task.assignee);
            probe.model = model.map(str::to_owned);
            probe.provider = provider.map(str::to_owned);
            profiles.validate(&probe)?;
        }
        let label = actor.label();
        self.write(|tx| {
            tx.execute(
                "UPDATE tasks SET model = ?2, provider = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, model, provider, now],
            )
            .map_err(sql)?;
            event(
                tx,
                Some(id),
                None,
                &label,
                "model_set",
                &json!({"model": model, "provider": provider}),
                now,
            )?;
            Ok(())
        })
    }

    /// Sends a task in review back to `ready` (or `todo` while a parent is
    /// open) without a reviewer's verdict, ending an open reviewer run.
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition`, `kanban.task_not_found`.
    pub fn reopen_review(
        &mut self,
        actor: &Actor,
        id: &str,
        reason: Option<&str>,
        now: i64,
    ) -> Result<Status, Error> {
        let label = actor.label();
        self.write(|tx| {
            let task = load(tx, id)?;
            let to = gate(tx, id)?;
            workflow::check(Verb::ReopenReview, actor.kind, task.status, to)?;
            let run = end_open_run(tx, id, "review_reopened", now)?;
            set_status(tx, id, to, now)?;
            if let Some(reason) = reason.filter(|r| !r.trim().is_empty()) {
                tx.execute(
                    "INSERT INTO comments(task_id, author, body, at) VALUES (?1, ?2, ?3, ?4)",
                    params![id, label, reason, now],
                )
                .map_err(sql)?;
            }
            let payload = json!({"status": to, "reason": reason, "assignee": task.assignee});
            event(
                tx,
                Some(id),
                run.as_deref(),
                &label,
                "review_reopened",
                &payload,
                now,
            )?;
            Ok(to)
        })
    }

    /// Replaces a `triage` task's body with its specification and moves it
    /// to `todo` (the promotion pass makes it `ready`).
    ///
    /// # Errors
    ///
    /// `kanban.illegal_transition` outside `triage`, `kanban.invalid_task`
    /// for a blank body.
    pub fn specify(&mut self, actor: &Actor, id: &str, body: &str, now: i64) -> Result<(), Error> {
        if body.trim().is_empty() {
            return Err(invalid("the specification is empty"));
        }
        let label = actor.label();
        self.write(|tx| {
            let task = load(tx, id)?;
            workflow::check(Verb::Specify, actor.kind, task.status, Status::Todo)?;
            tx.execute(
                "UPDATE tasks SET body = ?2 WHERE id = ?1",
                params![id, body.trim()],
            )
            .map_err(sql)?;
            set_status(tx, id, Status::Todo, now)?;
            event(
                tx,
                Some(id),
                None,
                &label,
                "specified",
                &json!({"chars": body.trim().chars().count()}),
                now,
            )?;
            Ok(())
        })
    }

    /// The open worker run whose ticket secret is `secret` (an external
    /// worker reporting through the CLI); the ticket is not consumed.
    ///
    /// # Errors
    ///
    /// `kanban.ticket_refused` when no open run matches.
    pub fn authenticate(&self, run: &str, secret: &str) -> Result<Run, Error> {
        let row = self
            .conn()
            .query_row(
                &format!(
                    "SELECT {} FROM task_runs WHERE id = ?1 AND ended_at IS NULL AND ticket_hash = ?2",
                    super::RUN_COLUMNS
                ),
                params![run, secret_hash(secret)],
                super::run_row,
            )
            .optional()
            .map_err(sql)?;
        row.ok_or_else(|| {
            Error::new(
                &codes::KANBAN_TICKET_REFUSED,
                "run ticket does not match an open run",
            )
        })
    }
}
