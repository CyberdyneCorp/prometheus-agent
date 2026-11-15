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
    /// `OWNER/REPO`, a pull-request URL or `local-only` (`None`).
    pub completion_contract: Option<String>,
    /// Run the worker in the goal loop.
    pub goal_mode: bool,
    /// Judged turns of the goal loop (`None`: 20).
    pub goal_max_turns: Option<u32>,
    /// The worker's model override.
    pub model: Option<String>,
    /// The override's provider (needs `model`).
    pub provider: Option<String>,
    /// Skills preloaded in the worker session.
    pub skills: Vec<String>,
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
            completion_contract: None,
            goal_mode: false,
            goal_max_turns: None,
            model: None,
            provider: None,
            skills: Vec::new(),
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

/// Which assignees exist and what a new task may name; the binary
/// answers from the home, its plugins and the creating profile.
pub trait Profiles {
    /// Whether `name` is a profile of the home.
    fn exists(&self, name: &str) -> bool;

    /// Whether `name` is a worker lane an enabled plugin declares.
    fn lane(&self, _name: &str) -> bool {
        false
    }

    /// Checks what only the binary knows: the model override against the
    /// provider registry, pinned skills against the assignee's skills.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task` naming the problem.
    fn validate(&self, _new: &NewTask) -> Result<(), Error> {
        Ok(())
    }

    /// The project bound to `board` (slug, primary folder) in the creating
    /// profile, which gives worktree tasks their branch convention.
    fn project(&self, _board: &str) -> Option<(String, std::path::PathBuf)> {
        None
    }
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

/// The refusal of an assignee or reviewer that is no profile (or lane).
#[must_use]
pub fn unknown_profile(name: &str) -> Error {
    Error::new(
        &codes::KANBAN_UNKNOWN_PROFILE,
        format!("unknown profile '{name}'"),
    )
}

/// Checks an assignee: a profile, or a worker lane; returns whether it
/// is a lane.
///
/// # Errors
///
/// `kanban.unknown_profile`.
pub fn assignee_lane(profiles: &dyn Profiles, name: &str) -> Result<bool, Error> {
    if profiles.exists(name) {
        Ok(false)
    } else if profiles.lane(name) {
        Ok(true)
    } else {
        Err(unknown_profile(name))
    }
}

/// The fields of the upgrades: the completion contract, the goal-loop
/// budget and the model override.
fn check_upgrades(new: &NewTask) -> Result<(), Error> {
    if let Some(contract) = &new.completion_contract {
        crate::contract::Contract::parse(contract)?;
    }
    if new.goal_max_turns == Some(0) {
        return Err(invalid("goal_max_turns must be at least 1"));
    }
    if new.provider.is_some() && new.model.is_none() {
        return Err(invalid("--provider requires --model"));
    }
    Ok(())
}

fn check(new: &NewTask, profiles: &dyn Profiles) -> Result<(), Error> {
    if new.title.trim().is_empty() {
        return Err(invalid("title is required"));
    }
    if let Some(assignee) = &new.assignee {
        assignee_lane(profiles, assignee)?;
    }
    if let Some(reviewer) = new.reviewer.as_deref().filter(|r| !profiles.exists(r)) {
        return Err(unknown_profile(reviewer));
    }
    check_upgrades(new)?;
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
    profiles.validate(new)
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
        let lane = match &new.assignee {
            Some(a) => assignee_lane(profiles, a)?,
            None => false,
        };
        let project = matches!(new.workspace, Workspace::Worktree { branch: None })
            .then(|| profiles.project(self.slug()))
            .flatten();
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
            extras(tx, &id, new, lane, project.as_ref())?;
            if let Some(run) = actor.run.as_deref() {
                crate::notify::inherit(tx, run, &id, now)?;
            }
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

pub(crate) fn insert(
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

/// The M3 upgrade fields of a new task.
pub(crate) fn extras(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    new: &NewTask,
    lane: bool,
    project: Option<&(String, std::path::PathBuf)>,
) -> Result<(), Error> {
    let contract = new
        .completion_contract
        .as_deref()
        .map(crate::contract::Contract::parse)
        .transpose()?
        .and_then(|c| c.stored());
    tx.execute(
        "UPDATE tasks SET completion_contract = ?2, goal_mode = ?3, goal_max_turns = ?4, model = ?5, \
         provider = ?6, skills = ?7, lane = ?8, project = ?9, repo_root = ?10 WHERE id = ?1",
        params![
            id,
            contract,
            new.goal_mode,
            new.goal_max_turns,
            new.model,
            new.provider,
            serde_json::to_string(&new.skills).unwrap_or_default(),
            lane,
            project.map(|p| p.0.as_str()),
            project.map(|p| p.1.to_string_lossy().into_owned()),
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
pub(crate) fn initial_status(
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
