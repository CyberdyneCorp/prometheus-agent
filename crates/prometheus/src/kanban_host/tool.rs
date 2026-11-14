//! The `kanban_*` tools (spec: kanban, Kanban toolset and grants). A
//! worker session's port carries its grant — its own board, task and run
//! — so every call is scoped to that board (`capability denied: kanban
//! board '<b>'` otherwise) and attaching works only on its own task; the
//! run-lifecycle tools refuse in any other session with `not a kanban
//! worker session`. Files are attached only when the session itself may
//! read them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_agent::tools::{KanbanPort, ReadCheck};
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_kanban::{Actor, BlockKind, Board, Boards, Completion, Filter, NewTask, Role, Status};
use pmt_tools::ToolResult;
use pmt_tools::agent_level_kanban::{NOT_WORKER, RUN_LIFECYCLE};
use serde_json::{Value, json};

use super::HomeProfiles;

/// A worker session's grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Its board.
    pub board: String,
    /// Its task.
    pub task: String,
    /// Its run.
    pub run: String,
    /// The profile it runs as.
    pub profile: String,
    /// Worker or reviewer.
    pub role: Role,
}

/// The port of a session of `scope` working in `cwd`, whose reads
/// `may_read` decides; `grant` makes it a worker's.
#[must_use]
pub fn port(
    scope: &ProfileScope,
    cwd: &Path,
    may_read: ReadCheck,
    grant: Option<Grant>,
) -> Arc<dyn KanbanPort> {
    let config = &scope.config().kanban;
    Arc::new(Port {
        home: scope.home().to_path_buf(),
        profile: scope.name().to_string(),
        current_board: config.current_board.clone(),
        limit: config.attachment_max_bytes,
        ttl_ms: super::ttl_ms(config),
        cwd: cwd.to_path_buf(),
        may_read,
        grant,
    })
}

struct Port {
    home: PathBuf,
    profile: String,
    current_board: Option<String>,
    limit: u64,
    ttl_ms: i64,
    cwd: PathBuf,
    may_read: ReadCheck,
    grant: Option<Grant>,
}

fn denied(message: impl Into<String>) -> Error {
    Error::new(&codes::KANBAN_NOT_PERMITTED, message)
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, Error> {
    text(args, key)
        .ok_or_else(|| Error::new(&codes::KANBAN_INVALID_TASK, format!("{key} is required")))
}

fn strings(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

impl KanbanPort for Port {
    fn call(&self, _session: SessionId, name: &str, arguments: Value) -> BoxFuture<'_, ToolResult> {
        let name = name.to_owned();
        Box::pin(async move {
            // Board writes are short SQLite transactions.
            match self.run(&name, &arguments) {
                Ok(value) => ToolResult::ok(value.to_string()),
                Err(err) => ToolResult::error(err.message().to_owned()),
            }
        })
    }
}

impl Port {
    fn actor(&self) -> Actor {
        match &self.grant {
            Some(g) if g.role == Role::Reviewer => Actor::reviewer(&g.profile),
            Some(g) => Actor::worker(&g.profile, &g.run),
            None => Actor::user(&self.profile),
        }
    }

    fn board(&self, args: &Value) -> Result<Board, Error> {
        let boards = Boards::open(&self.home)?;
        let wanted = text(args, "board");
        let slug = match (&self.grant, wanted) {
            (Some(g), Some(b))
                if pmt_kanban::boards::slug(b).ok().as_deref() != Some(g.board.as_str()) =>
            {
                return Err(denied(format!("capability denied: kanban board '{b}'")));
            }
            (Some(g), _) => g.board.clone(),
            (None, wanted) => boards.resolve(wanted, self.current_board.as_deref())?,
        };
        boards.board(&slug)
    }

    fn task_id(&self, args: &Value) -> Result<String, Error> {
        match (text(args, "task_id"), &self.grant) {
            (Some(id), _) => Ok(id.to_owned()),
            (None, Some(g)) => Ok(g.task.clone()),
            (None, None) => required(args, "task_id").map(str::to_owned),
        }
    }

    fn run(&self, name: &str, args: &Value) -> Result<Value, Error> {
        if self.grant.is_some() && NOT_WORKER.contains(&name) {
            return Err(denied(format!(
                "capability denied: {name} is not part of a worker's grant"
            )));
        }
        if self.grant.is_none() && RUN_LIFECYCLE.contains(&name) {
            return Err(denied("not a kanban worker session"));
        }
        let mut board = self.board(args)?;
        let now = pmt_kanban::now_ms();
        match name {
            "kanban_show" => show(&board, &self.task_id(args)?),
            "kanban_list" => list(&board, args),
            "kanban_attachments" => attachments(&board, &self.task_id(args)?),
            "kanban_create" => self.create(&mut board, args, now),
            "kanban_comment" => {
                let id = self.task_id(args)?;
                board.comment(&self.actor(), &id, required(args, "body")?, now)?;
                Ok(json!({"task_id": id, "commented": true}))
            }
            "kanban_attach" => self.attach(&mut board, args, now),
            "kanban_link" => {
                board.link(
                    &self.actor(),
                    required(args, "parent")?,
                    required(args, "child")?,
                    None,
                    now,
                )?;
                Ok(json!({"linked": true}))
            }
            "kanban_unblock" => {
                let id = required(args, "task_id")?;
                let status = board.unblock(&self.actor(), id, now)?;
                Ok(json!({"task_id": id, "status": status}))
            }
            "kanban_request_changes" => {
                let id = self.task_id(args)?;
                board.request_changes(&self.actor(), &id, required(args, "comment")?, now)?;
                Ok(json!({"task_id": id, "status": Status::Todo}))
            }
            _ => self.lifecycle(&mut board, name, args, now),
        }
    }

    /// The run-lifecycle tools of a worker session.
    fn lifecycle(
        &self,
        board: &mut Board,
        name: &str,
        args: &Value,
        now: i64,
    ) -> Result<Value, Error> {
        let grant = self
            .grant
            .as_ref()
            .ok_or_else(|| denied("not a kanban worker session"))?;
        let actor = self.actor();
        let id = grant.task.clone();
        let status = match name {
            "kanban_complete" if grant.role == Role::Reviewer => {
                board.approve(
                    &actor,
                    &id,
                    text(args, "summary").or(text(args, "result")),
                    now,
                )?;
                Status::Done
            }
            "kanban_complete" => {
                let completion = Completion {
                    result: text(args, "result").map(str::to_owned),
                    summary: text(args, "summary").map(str::to_owned),
                    created_tasks: strings(args, "created_tasks"),
                    attachment_limit: self.limit,
                    ..Completion::default()
                };
                board.complete(&actor, &id, &completion, now)?
            }
            "kanban_block" => {
                let kind = BlockKind::parse(required(args, "kind")?)?;
                board.block(&actor, &id, kind, required(args, "reason")?, now)?
            }
            "kanban_schedule" => {
                let wake = text(args, "wake_at").map(parse_instant).transpose()?;
                board.schedule(&actor, &id, wake, now)?;
                Status::Scheduled
            }
            "kanban_heartbeat" => {
                board.heartbeat(&actor, &grant.run, text(args, "note"), self.ttl_ms, now)?;
                return Ok(json!({"task_id": id, "renewed": true}));
            }
            "kanban_request_review" => {
                board.request_review(&actor, &id, text(args, "summary"), now)?;
                Status::Review
            }
            other => return Err(denied(format!("unknown kanban tool '{other}'"))),
        };
        Ok(json!({"task_id": id, "status": status}))
    }

    fn create(&self, board: &mut Board, args: &Value, now: i64) -> Result<Value, Error> {
        let mut new = NewTask::titled(required(args, "title")?);
        text(args, "body")
            .unwrap_or_default()
            .clone_into(&mut new.body);
        new.assignee = text(args, "assignee").map(str::to_owned);
        new.parents = strings(args, "parents");
        new.priority = args.get("priority").and_then(Value::as_i64).unwrap_or(0);
        new.idempotency_key = text(args, "idempotency_key").map(str::to_owned);
        let created = board.create(&self.actor(), &new, &HomeProfiles(self.home.clone()), now)?;
        Ok(serde_json::to_value(created).unwrap_or_default())
    }

    fn attach(&self, board: &mut Board, args: &Value, now: i64) -> Result<Value, Error> {
        let id = self.task_id(args)?;
        if let Some(grant) = &self.grant
            && grant.task != id
        {
            return Err(denied(format!(
                "capability denied: a worker attaches only to its own task {}",
                grant.task
            )));
        }
        let raw = PathBuf::from(required(args, "path")?);
        let path = if raw.is_absolute() {
            raw
        } else {
            self.cwd.join(raw)
        };
        if !(self.may_read)(&path) {
            return Err(denied(format!(
                "capability denied: read {}",
                path.display()
            )));
        }
        let attachment = board.attach_file(&self.actor(), &id, &path, self.limit, now)?;
        Ok(serde_json::to_value(attachment).unwrap_or_default())
    }
}

fn parse_instant(text: &str) -> Result<i64, Error> {
    text.parse::<jiff::Timestamp>()
        .map(jiff::Timestamp::as_millisecond)
        .map_err(|e| {
            Error::new(
                &codes::KANBAN_INVALID_TASK,
                format!("wake_at must be an RFC 3339 instant: {e}"),
            )
        })
}

/// A task as `kanban_show` and `kanban show --json` report it.
///
/// # Errors
///
/// `kanban.task_not_found`.
pub fn show(board: &Board, id: &str) -> Result<Value, Error> {
    let task = board.task(id)?;
    let (usd, tokens) = board.spend(id)?;
    Ok(json!({
        "board": board.slug(),
        "task": task,
        "parents": board.parents(id)?,
        "children": board.children(id)?,
        "comments": board.comments(id)?,
        "attachments": board.attachments(id)?,
        "runs": board.runs(id)?,
        "spend": {"usd": usd, "tokens": tokens},
    }))
}

fn list(board: &Board, args: &Value) -> Result<Value, Error> {
    let filter = Filter {
        status: text(args, "status").map(Status::parse).transpose()?,
        assignee: text(args, "assignee").map(str::to_owned),
        archived: false,
    };
    let tasks: Vec<Value> = board
        .tasks(&filter)?
        .into_iter()
        .map(|t| json!({"id": t.id, "title": t.title, "status": t.status, "assignee": t.assignee, "priority": t.priority}))
        .collect();
    Ok(json!({"board": board.slug(), "tasks": tasks}))
}

fn attachments(board: &Board, id: &str) -> Result<Value, Error> {
    let rows: Vec<Value> = board
        .attachments(id)?
        .into_iter()
        .map(|a| {
            let path = board.attachment_path(&a);
            json!({"id": a.id, "filename": a.filename, "content_type": a.content_type, "size": a.size, "uploader": a.uploader, "path": path})
        })
        .collect();
    Ok(json!({"task_id": id, "attachments": rows}))
}
