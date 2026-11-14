//! Task creation (spec: kanban, Task creation; Idempotent create).

use pmt_core::error::{Error, codes};
use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;
use serde_json::json;

use super::{Board, event, open_parents};
use crate::db::sql;
use crate::model::{Actor, Status, TaskKind, Workspace, invalid};

/// What creating a task names.
#[derive(Debug, Clone)]
pub struct NewTask {
    /// Required, non-blank.
    pub title: String,
    /// The body.
    pub body: String,
    /// The worker profile.
    pub assignee: Option<String>,
    /// The reviewer profile.
    pub reviewer: Option<String>,
    /// Parent task ids on this board.
    pub parents: Vec<String>,
    /// Where the worker works.
    pub workspace: Workspace,
    /// What the worker does.
    pub kind: TaskKind,
    /// Higher is claimed first.
    pub priority: i64,
    /// `triage`, `todo` or `blocked`; `None` derives it from the parents.
    pub initial_status: Option<Status>,
    /// Returns the existing task instead of a duplicate.
    pub idempotency_key: Option<String>,
    /// Per-task review lane.
    pub review: Option<bool>,
    /// Failures before the breaker trips.
    pub max_retries: Option<u32>,
    /// USD limit.
    pub budget_usd: Option<f64>,
    /// Token limit.
    pub budget_tokens: Option<u64>,
    /// Files the worker must leave in its workspace.
    pub artifacts: Vec<String>,
}

impl NewTask {
    /// A task titled `title` with every other field defaulted.
    #[must_use]
    pub fn titled(title: &str) -> Self {
        Self {
            title: title.to_owned(),
            body: String::new(),
            assignee: None,
            reviewer: None,
            parents: Vec::new(),
            workspace: Workspace::Scratch,
            kind: TaskKind::Agent,
            priority: 0,
            initial_status: None,
            idempotency_key: None,
            review: None,
            max_retries: None,
            budget_usd: None,
            budget_tokens: None,
            artifacts: Vec::new(),
        }
    }
}

/// The result of `create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Created {
    /// The task id (the existing one for a repeated idempotency key).
    pub id: String,
    /// Whether a task was inserted.
    pub created: bool,
    /// Its status.
    pub status: Status,
}

/// Which profiles exist; the binary answers from the home.
pub trait Profiles {
    /// Whether `name` is a profile of the home.
    fn exists(&self, name: &str) -> bool;
}

impl<F: Fn(&str) -> bool> Profiles for F {
    fn exists(&self, name: &str) -> bool {
        self(name)
    }
}

/// A fresh `t_` + 8 lowercase hex id.
pub(crate) fn task_id() -> String {
    let mut bytes = [0_u8; 4];
    // The OS generator does not fail in practice; a zero id only collides
    // and is retried by the caller.
    let _ = getrandom::fill(&mut bytes);
    format!("t_{:08x}", u32::from_be_bytes(bytes))
}

fn check(new: &NewTask, profiles: &dyn Profiles) -> Result<(), Error> {
    if new.title.trim().is_empty() {
        return Err(invalid("title is required"));
    }
    for profile in new.assignee.iter().chain(new.reviewer.iter()) {
        if !profiles.exists(profile) {
            return Err(Error::new(
                &codes::KANBAN_UNKNOWN_PROFILE,
                format!("unknown profile '{profile}'"),
            ));
        }
    }
    if let Some(status) = new.initial_status
        && !matches!(status, Status::Triage | Status::Todo | Status::Blocked)
    {
        return Err(invalid(format!(
            "initial_status must be triage, todo or blocked (not {status})"
        )));
    }
    match &new.kind {
        TaskKind::Eval { suite, .. } if suite.trim().is_empty() => {
            return Err(invalid("an eval task needs a suite"));
        }
        TaskKind::Evolve {
            target_task,
            budget_k,
            ..
        } if target_task.trim().is_empty() || *budget_k == 0 => {
            return Err(invalid(
                "an evolve task needs target_task and budget_k >= 1",
            ));
        }
        _ => {}
    }
    if new.budget_usd.is_some_and(|b| !b.is_finite() || b < 0.0) {
        return Err(invalid("budget_usd must be a non-negative number"));
    }
    for artifact in &new.artifacts {
        if artifact.is_empty()
            || artifact.starts_with('/')
            || artifact.split('/').any(|p| p == "..")
        {
            return Err(invalid(format!(
                "artifact '{artifact}' must be a relative path inside the workspace"
            )));
        }
    }
    Ok(())
}

impl Board {
    /// Creates a task, or returns the live task holding the same
    /// idempotency key.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task` (blank title, unknown parents, bad fields),
    /// `kanban.unknown_profile`; nothing is created on error.
    pub fn create(
        &mut self,
        actor: &Actor,
        new: &NewTask,
        profiles: &dyn Profiles,
        now: i64,
    ) -> Result<Created, Error> {
        check(new, profiles)?;
        let label = actor.label();
        self.write(|tx| {
            if let Some(found) = existing(tx, new.idempotency_key.as_deref())? {
                return Ok(found);
            }
            known_parents(tx, &new.parents)?;
            let mut id = task_id();
            while super::load(tx, &id).is_ok() {
                id = task_id();
            }
            insert(tx, &id, new, &label, actor.run.as_deref(), now)?;
            let (status, open) = initial_status(tx, &id, new)?;
            let run = actor.run.as_deref();
            event(
                tx,
                Some(&id),
                run,
                &label,
                "created",
                &json!({"title": new.title.trim(), "status": status, "parents": new.parents}),
                now,
            )?;
            if status == Status::Todo && !open.is_empty() {
                event(
                    tx,
                    Some(&id),
                    run,
                    &label,
                    "dependency_wait",
                    &json!({"parents": open}),
                    now,
                )?;
            }
            Ok(Created {
                id,
                created: true,
                status,
            })
        })
    }
}

fn insert(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    new: &NewTask,
    label: &str,
    run: Option<&str>,
    now: i64,
) -> Result<(), Error> {
    tx.execute(
                "INSERT INTO tasks(id, title, body, status, assignee, reviewer, priority, workspace, \
                 kind, review, idempotency_key, max_retries, budget_usd, budget_tokens, artifacts, \
                 created_by, created_run, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, 'triage', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?17)",
                params![
                    id,
                    new.title.trim(),
                    new.body,
                    new.assignee,
                    new.reviewer,
                    new.priority,
                    serde_json::to_string(&new.workspace).unwrap_or_default(),
                    serde_json::to_string(&new.kind).unwrap_or_default(),
                    new.review,
                    new.idempotency_key,
                    new.max_retries,
                    new.budget_usd,
                    new.budget_tokens.and_then(|t| i64::try_from(t).ok()),
                    serde_json::to_string(&new.artifacts).unwrap_or_default(),
                    label,
                    run,
                    now,
                ],
            )
            .map_err(sql)?;
    Ok(())
}

/// The live task holding idempotency `key`, as a repeated create reports
/// it.
fn existing(tx: &rusqlite::Transaction<'_>, key: Option<&str>) -> Result<Option<Created>, Error> {
    let Some(key) = key else {
        return Ok(None);
    };
    let found: Option<(String, String)> = tx
        .query_row(
            "SELECT id, status FROM tasks WHERE idempotency_key = ?1 AND status != 'archived'",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql)?;
    found
        .map(|(id, status)| {
            Ok(Created {
                id,
                created: false,
                status: Status::parse(&status)?,
            })
        })
        .transpose()
}

/// Refuses parents that are not tasks of the board.
fn known_parents(tx: &rusqlite::Transaction<'_>, parents: &[String]) -> Result<(), Error> {
    let unknown: Vec<&str> = parents
        .iter()
        .filter(|p| super::load(tx, p).is_err())
        .map(String::as_str)
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(invalid(format!(
            "unknown parent task(s): {}",
            unknown.join(", ")
        )))
    }
}

/// Links task `id` to its parents and sets its first status: the given
/// one, else `todo` while a parent is open, else `ready`. Returns it and
/// the open parents.
fn initial_status(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    new: &NewTask,
) -> Result<(Status, Vec<String>), Error> {
    for parent in &new.parents {
        tx.execute(
            "INSERT OR IGNORE INTO links(parent, child) VALUES (?1, ?2)",
            params![parent, id],
        )
        .map_err(sql)?;
    }
    let open = open_parents(tx, id)?;
    let status = new.initial_status.unwrap_or(if open.is_empty() {
        Status::Ready
    } else {
        Status::Todo
    });
    tx.execute(
        "UPDATE tasks SET status = ?2 WHERE id = ?1",
        params![id, status.as_str()],
    )
    .map_err(sql)?;
    if status == Status::Blocked {
        // Created blocked: a sticky human block the promotion pass never
        // lifts.
        tx.execute(
            "UPDATE tasks SET block_kind = 'needs_input', block_reason = 'created blocked', \
             blocked_from = 'todo' WHERE id = ?1",
            [id],
        )
        .map_err(sql)?;
    }
    Ok((status, open))
}
