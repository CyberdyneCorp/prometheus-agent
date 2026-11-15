//! One open board: reads, the event log and the transaction every
//! mutation runs in. The operations live in siblings by topic: [`create`]
//! (tasks and idempotency), [`links`] (dependencies), [`flow`]
//! (promotion, claims, blocks, schedules), [`finish`] (completion gates,
//! review lane, archive), [`runs`] (heartbeats, run outcomes and the
//! failure breaker).

pub mod create;
pub mod finish;
pub mod flow;
mod gates;
pub mod links;
pub mod manage;
pub mod runs;
pub mod terminate;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde_json::Value;

use crate::boards::{BoardSettings, Boards};
use crate::contract::PrChecks;
use crate::db::{self, sql};
use crate::model::{Actor, BlockKind, Comment, Event, Run, Status, Task, TaskKind, Workspace};
use crate::observe::Observer;

/// An open board.
pub struct Board {
    slug: String,
    dir: PathBuf,
    blobs: PathBuf,
    settings: BoardSettings,
    conn: Connection,
    pr_checks: Option<Arc<dyn PrChecks>>,
    observer: Option<Arc<dyn Observer>>,
}

impl std::fmt::Debug for Board {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Board")
            .field("slug", &self.slug)
            .finish_non_exhaustive()
    }
}

pub(crate) const TASK_COLUMNS: &str = "id, title, body, status, assignee, reviewer, priority, \
    workspace, workspace_path, kind, review, idempotency_key, block_kind, block_reason, wake_at, \
    not_before, consecutive_failures, last_failure, max_retries, budget_usd, budget_tokens, \
    artifacts, result, summary, created_by, created_run, created_at, updated_at, completed_at, \
    completion_contract, pr_url, goal_mode, goal_max_turns, model, provider, skills, lane, project, \
    repo_root";

pub(crate) const RUN_COLUMNS: &str = "id, task_id, profile, role, started_at, ended_at, outcome, \
    exit_code, session_id, trajectory_id, cost_usd, tokens, pid, pid_start, lease_expires_at, \
    heartbeat_at, error";

fn text_err(e: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.to_string().into())
}

pub(crate) fn task_row(row: &Row<'_>) -> rusqlite::Result<Task> {
    let status: String = row.get(3)?;
    let workspace: String = row.get(7)?;
    let kind: String = row.get(9)?;
    let block_kind: Option<String> = row.get(12)?;
    let artifacts: String = row.get(21)?;
    let skills: String = row.get(35)?;
    Ok(Task {
        id: row.get(0)?,
        title: row.get(1)?,
        body: row.get(2)?,
        status: Status::parse(&status).map_err(|e| text_err(e.message()))?,
        assignee: row.get(4)?,
        reviewer: row.get(5)?,
        priority: row.get(6)?,
        workspace: serde_json::from_str::<Workspace>(&workspace).map_err(text_err)?,
        workspace_path: row.get::<_, Option<String>>(8)?.map(PathBuf::from),
        kind: serde_json::from_str::<TaskKind>(&kind).map_err(text_err)?,
        review: row.get(10)?,
        idempotency_key: row.get(11)?,
        block_kind: block_kind
            .map(|k| BlockKind::parse(&k).map_err(|e| text_err(e.message())))
            .transpose()?,
        block_reason: row.get(13)?,
        wake_at: row.get(14)?,
        not_before: row.get(15)?,
        consecutive_failures: row.get(16)?,
        last_failure: row.get(17)?,
        max_retries: row.get(18)?,
        budget_usd: row.get(19)?,
        budget_tokens: row
            .get::<_, Option<i64>>(20)?
            .and_then(|t| u64::try_from(t).ok()),
        artifacts: serde_json::from_str(&artifacts).map_err(text_err)?,
        result: row.get(22)?,
        summary: row.get(23)?,
        created_by: row.get(24)?,
        created_run: row.get(25)?,
        created_at: row.get(26)?,
        updated_at: row.get(27)?,
        completed_at: row.get(28)?,
        completion_contract: row.get(29)?,
        pr_url: row.get(30)?,
        goal_mode: row.get(31)?,
        goal_max_turns: row.get(32)?,
        model: row.get(33)?,
        provider: row.get(34)?,
        skills: serde_json::from_str(&skills).map_err(text_err)?,
        lane: row.get(36)?,
        project: row.get(37)?,
        repo_root: row.get::<_, Option<String>>(38)?.map(PathBuf::from),
    })
}

pub(crate) fn run_row(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: row.get(0)?,
        task_id: row.get(1)?,
        profile: row.get(2)?,
        role: row.get(3)?,
        started_at: row.get(4)?,
        ended_at: row.get(5)?,
        outcome: row.get(6)?,
        exit_code: row.get(7)?,
        session_id: row.get(8)?,
        trajectory_id: row.get(9)?,
        cost_usd: row.get(10)?,
        tokens: row
            .get::<_, i64>(11)
            .map(|t| u64::try_from(t).unwrap_or(0))?,
        pid: row.get(12)?,
        pid_start: row
            .get::<_, Option<i64>>(13)?
            .and_then(|t| u64::try_from(t).ok()),
        lease_expires_at: row.get(14)?,
        heartbeat_at: row.get(15)?,
        error: row.get(16)?,
    })
}

fn event_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    let payload: String = row.get(5)?;
    Ok(Event {
        id: row.get(0)?,
        task_id: row.get(1)?,
        run_id: row.get(2)?,
        actor: row.get(3)?,
        kind: row.get(4)?,
        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
        at: row.get(6)?,
    })
}

fn events_after(conn: &Connection, after: i64) -> Result<Vec<Event>, Error> {
    let mut stmt = conn
        .prepare("SELECT id, task_id, run_id, actor, kind, payload, at FROM events WHERE id > ?1 ORDER BY id")
        .map_err(sql)?;
    let rows = stmt
        .query_map([after], event_row)
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    Ok(rows)
}

/// The refusal for an unknown task id.
#[must_use]
pub fn task_not_found(id: &str) -> Error {
    Error::new(
        &codes::KANBAN_TASK_NOT_FOUND,
        format!("task '{id}' not found"),
    )
}

/// Appends one event inside `tx`.
pub(crate) fn event(
    tx: &Transaction<'_>,
    task: Option<&str>,
    run: Option<&str>,
    actor: &str,
    kind: &str,
    payload: &Value,
    now: i64,
) -> Result<i64, Error> {
    tx.execute(
        "INSERT INTO events(task_id, run_id, actor, kind, payload, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![task, run, actor, kind, payload.to_string(), now],
    )
    .map_err(sql)?;
    Ok(tx.last_insert_rowid())
}

/// Reads a task inside `tx`.
pub(crate) fn load(tx: &Connection, id: &str) -> Result<Task, Error> {
    tx.query_row(
        &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
        [id],
        task_row,
    )
    .optional()
    .map_err(sql)?
    .ok_or_else(|| task_not_found(id))
}

/// Sets a task's status (and `updated_at`) inside `tx`.
pub(crate) fn set_status(
    tx: &Transaction<'_>,
    id: &str,
    to: Status,
    now: i64,
) -> Result<(), Error> {
    tx.execute(
        "UPDATE tasks SET status = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, to.as_str(), now],
    )
    .map_err(sql)?;
    Ok(())
}

/// The ids of `id`'s parents that are not done or archived.
pub(crate) fn open_parents(tx: &Connection, id: &str) -> Result<Vec<String>, Error> {
    let mut stmt = tx
        .prepare(
            "SELECT p.id FROM links l JOIN tasks p ON p.id = l.parent \
             WHERE l.child = ?1 AND p.status NOT IN ('done', 'archived') ORDER BY p.id",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map([id], |r| r.get(0))
        .map_err(sql)?
        .collect::<Result<Vec<String>, _>>()
        .map_err(sql)?;
    Ok(rows)
}

/// The task's open run.
pub(crate) fn open_run(tx: &Connection, task: &str) -> Result<Option<Run>, Error> {
    tx.query_row(
        &format!("SELECT {RUN_COLUMNS} FROM task_runs WHERE task_id = ?1 AND ended_at IS NULL"),
        [task],
        run_row,
    )
    .optional()
    .map_err(sql)
}

/// Filters of [`Board::tasks`].
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Only these statuses.
    pub status: Option<Status>,
    /// Only this assignee.
    pub assignee: Option<String>,
    /// Include archived tasks.
    pub archived: bool,
}

impl Board {
    pub(crate) fn open(
        boards: &Boards,
        slug: &str,
        settings: BoardSettings,
    ) -> Result<Self, Error> {
        let dir = boards.dir(slug);
        let conn = db::open(&dir.join("board.db"))?;
        Ok(Self {
            slug: slug.to_owned(),
            dir,
            blobs: boards.blobs_dir(),
            settings,
            conn,
            pr_checks: None,
            observer: None,
        })
    }

    /// Reads pull-request evidence for completion contracts with `checks`.
    #[must_use]
    pub fn with_pr_checks(mut self, checks: Arc<dyn PrChecks>) -> Self {
        self.pr_checks = Some(checks);
        self
    }

    /// Hands every committed transaction's events to `observer`.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub(crate) fn pr_checks(&self) -> Option<&Arc<dyn PrChecks>> {
        self.pr_checks.as_ref()
    }

    /// The board's slug.
    #[must_use]
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// `<home>/kanban/boards/<slug>`.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The shared blob store.
    #[must_use]
    pub fn blobs(&self) -> &Path {
        &self.blobs
    }

    /// `board.toml`.
    #[must_use]
    pub fn settings(&self) -> &BoardSettings {
        &self.settings
    }

    /// `<board>/workspaces/<task>`.
    #[must_use]
    pub fn scratch_dir(&self, task: &str) -> PathBuf {
        self.dir.join("workspaces").join(task)
    }

    /// `<board>/logs`.
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    /// Runs `body` in one immediate transaction; an observer then sees
    /// the events it appended (the transaction is the only writer, so
    /// every event after the first id is its own).
    pub(crate) fn write<T>(
        &mut self,
        body: impl FnOnce(&Transaction<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)?;
        let before: Option<i64> = match self.observer {
            Some(_) => Some(
                tx.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))
                    .map_err(sql)?,
            ),
            None => None,
        };
        let out = body(&tx)?;
        let appended = match before {
            Some(after) => events_after(&tx, after)?,
            None => Vec::new(),
        };
        tx.commit().map_err(sql)?;
        if let (Some(observer), false) = (self.observer.clone(), appended.is_empty()) {
            observer.observe(self, &appended);
        }
        Ok(out)
    }

    /// Records an event outside any other change (a refusal, a summary).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn record(
        &mut self,
        task: Option<&str>,
        run: Option<&str>,
        actor: &Actor,
        kind: &str,
        payload: &Value,
        now: i64,
    ) -> Result<i64, Error> {
        let label = actor.label();
        self.write(|tx| event(tx, task, run, &label, kind, payload, now))
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// One task.
    ///
    /// # Errors
    ///
    /// `kanban.task_not_found`.
    pub fn task(&self, id: &str) -> Result<Task, Error> {
        load(&self.conn, id)
    }

    /// Tasks by priority then age.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn tasks(&self, filter: &Filter) -> Result<Vec<Task>, Error> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks WHERE (?1 IS NULL OR status = ?1) \
                 AND (?2 IS NULL OR assignee = ?2) AND (?3 OR status != 'archived') \
                 ORDER BY priority DESC, created_at, id"
            ))
            .map_err(sql)?;
        let rows = stmt
            .query_map(
                params![
                    filter.status.map(Status::as_str),
                    filter.assignee,
                    filter.archived
                ],
                task_row,
            )
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(rows)
    }

    /// A task's parents.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn parents(&self, id: &str) -> Result<Vec<String>, Error> {
        self.ids(
            "SELECT parent FROM links WHERE child = ?1 ORDER BY parent",
            id,
        )
    }

    /// A task's children.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn children(&self, id: &str) -> Result<Vec<String>, Error> {
        self.ids(
            "SELECT child FROM links WHERE parent = ?1 ORDER BY child",
            id,
        )
    }

    fn ids(&self, query: &str, id: &str) -> Result<Vec<String>, Error> {
        let mut stmt = self.conn.prepare(query).map_err(sql)?;
        let rows = stmt
            .query_map([id], |r| r.get(0))
            .map_err(sql)?
            .collect::<Result<Vec<String>, _>>()
            .map_err(sql)?;
        Ok(rows)
    }

    /// Every link `(parent, child)`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn links(&self) -> Result<Vec<(String, String)>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT parent, child FROM links ORDER BY parent, child")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(rows)
    }

    /// A task's events (all board events with `None`), oldest first.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn events(&self, task: Option<&str>) -> Result<Vec<Event>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, run_id, actor, kind, payload, at FROM events \
                 WHERE (?1 IS NULL OR task_id = ?1) ORDER BY id",
            )
            .map_err(sql)?;
        let rows = stmt
            .query_map([task], event_row)
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(rows)
    }

    /// The board's last `limit` events, newest first.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn recent_events(&self, limit: usize) -> Result<Vec<Event>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, run_id, actor, kind, payload, at FROM events \
                 ORDER BY id DESC LIMIT ?1",
            )
            .map_err(sql)?;
        let rows = stmt
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], event_row)
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(rows)
    }

    /// Events after `after` of the given kinds, oldest first.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn events_since(&self, after: i64, kinds: &[&str]) -> Result<Vec<Event>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, run_id, actor, kind, payload, at FROM events \
                 WHERE id > ?1 ORDER BY id",
            )
            .map_err(sql)?;
        let rows = stmt
            .query_map([after], event_row)
            .map_err(sql)?
            .filter_map(Result::ok)
            .filter(|e| kinds.is_empty() || kinds.contains(&e.kind.as_str()))
            .collect();
        Ok(rows)
    }

    /// A task's runs, oldest first.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn runs(&self, task: &str) -> Result<Vec<Run>, Error> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM task_runs WHERE task_id = ?1 ORDER BY started_at, id"
            ))
            .map_err(sql)?;
        let rows = stmt
            .query_map([task], run_row)
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(rows)
    }

    /// One run.
    ///
    /// # Errors
    ///
    /// `kanban.run_not_found`.
    pub fn run(&self, id: &str) -> Result<Run, Error> {
        self.conn
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM task_runs WHERE id = ?1"),
                [id],
                run_row,
            )
            .optional()
            .map_err(sql)?
            .ok_or_else(|| {
                Error::new(
                    &codes::KANBAN_RUN_NOT_FOUND,
                    format!("run '{id}' not found"),
                )
            })
    }

    /// Every open run.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn open_runs(&self) -> Result<Vec<Run>, Error> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM task_runs WHERE ended_at IS NULL ORDER BY started_at"
            ))
            .map_err(sql)?;
        let rows = stmt
            .query_map([], run_row)
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(rows)
    }

    /// A task's comments, oldest first.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn comments(&self, task: &str) -> Result<Vec<Comment>, Error> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, author, body, at FROM comments WHERE task_id = ?1 ORDER BY id",
            )
            .map_err(sql)?;
        let rows = stmt
            .query_map([task], |r| {
                Ok(Comment {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    author: r.get(2)?,
                    body: r.get(3)?,
                    at: r.get(4)?,
                })
            })
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(rows)
    }

    /// Adds a comment.
    ///
    /// # Errors
    ///
    /// `kanban.task_not_found`, `kanban.invalid_task` for a blank body.
    pub fn comment(&mut self, actor: &Actor, id: &str, body: &str, now: i64) -> Result<i64, Error> {
        if body.trim().is_empty() {
            return Err(crate::model::invalid("comment is required"));
        }
        let label = actor.label();
        self.write(|tx| {
            load(tx, id)?;
            tx.execute(
                "INSERT INTO comments(task_id, author, body, at) VALUES (?1, ?2, ?3, ?4)",
                params![id, label, body, now],
            )
            .map_err(sql)?;
            let comment = tx.last_insert_rowid();
            event(
                tx,
                Some(id),
                actor.run.as_deref(),
                &label,
                "commented",
                &serde_json::json!({"comment": comment}),
                now,
            )?;
            Ok(comment)
        })
    }

    /// Changes editable fields (title, body, priority, reviewer, review).
    ///
    /// # Errors
    ///
    /// `kanban.task_not_found`, `kanban.invalid_task`.
    pub fn edit(&mut self, actor: &Actor, id: &str, edit: &Edit, now: i64) -> Result<(), Error> {
        if edit.title.as_deref().is_some_and(|t| t.trim().is_empty()) {
            return Err(crate::model::invalid("title is required"));
        }
        let label = actor.label();
        self.write(|tx| {
            load(tx, id)?;
            tx.execute(
                "UPDATE tasks SET title = COALESCE(?2, title), body = COALESCE(?3, body), \
                 priority = COALESCE(?4, priority), updated_at = ?5 WHERE id = ?1",
                params![
                    id,
                    edit.title.as_deref().map(str::trim),
                    edit.body,
                    edit.priority,
                    now
                ],
            )
            .map_err(sql)?;
            if let Some(reviewer) = &edit.reviewer {
                tx.execute(
                    "UPDATE tasks SET reviewer = ?2 WHERE id = ?1",
                    params![id, reviewer],
                )
                .map_err(sql)?;
            }
            if let Some(review) = edit.review {
                tx.execute(
                    "UPDATE tasks SET review = ?2 WHERE id = ?1",
                    params![id, review],
                )
                .map_err(sql)?;
            }
            event(
                tx,
                Some(id),
                None,
                &label,
                "edited",
                &serde_json::to_value(edit).unwrap_or_default(),
                now,
            )?;
            Ok(())
        })
    }

    /// Assigns (or unassigns) a task to a profile or worker lane.
    ///
    /// # Errors
    ///
    /// `kanban.task_not_found`, `kanban.unknown_profile`.
    pub fn assign(
        &mut self,
        actor: &Actor,
        id: &str,
        profile: Option<&str>,
        profiles: &dyn create::Profiles,
        now: i64,
    ) -> Result<(), Error> {
        let lane = match profile {
            Some(p) => create::assignee_lane(profiles, p)?,
            None => false,
        };
        let label = actor.label();
        self.write(|tx| {
            load(tx, id)?;
            tx.execute(
                "UPDATE tasks SET assignee = ?2, lane = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, profile, lane, now],
            )
            .map_err(sql)?;
            event(
                tx,
                Some(id),
                None,
                &label,
                "assigned",
                &serde_json::json!({"assignee": profile}),
                now,
            )?;
            Ok(())
        })
    }

    /// Cumulative spend of every run of a task: (USD, tokens).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn spend(&self, task: &str) -> Result<(f64, u64), Error> {
        self.conn
            .query_row(
                "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(tokens), 0) FROM task_runs WHERE task_id = ?1",
                [task],
                |r| Ok((r.get::<_, f64>(0)?, r.get::<_, i64>(1)?)),
            )
            .map(|(usd, tokens)| (usd, u64::try_from(tokens).unwrap_or(0)))
            .map_err(sql)
    }
}

/// Editable task fields; `None` leaves a field as it is.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Edit {
    /// New title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// New body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// New priority.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    /// New reviewer (`Some(None)` clears it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<Option<String>>,
    /// New review-lane switch (`Some(None)`: the board's default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<Option<bool>>,
}
