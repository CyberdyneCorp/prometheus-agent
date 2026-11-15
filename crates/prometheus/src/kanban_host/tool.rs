//! The `kanban_*` tools (spec: kanban, Kanban toolset and grants; URL
//! attachments and attachment listing tools; PR completion contracts;
//! Goal-mode cards). A worker session's port carries its grant — its own
//! board, task and run — so every call is scoped to that board
//! (`capability denied: kanban board '<b>'` otherwise) and attaching works
//! only on its own task; the run-lifecycle tools refuse in any other
//! session with `not a kanban worker session`. Files are attached only
//! when the session itself may read them; URLs are fetched server-side
//! under the profile's egress policy (private, loopback and link-local
//! destinations refused) and capped at `kanban.attachment_max_bytes`.
//! A goal-mode worker's `kanban_complete` and `kanban_request_review` ask
//! the session's goal judge first.

use std::path::PathBuf;
use std::sync::Arc;

use pmt_agent::tools::{KanbanPort, ReadCheck};
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_kanban::{Actor, BlockKind, Board, Completion, Filter, Handoff, NewTask, Role, Status};
use pmt_tools::ToolResult;
use pmt_tools::agent_level_kanban::{NOT_WORKER, RUN_LIFECYCLE};
use serde_json::{Value, json};

use super::goal::GoalJudge;

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

/// What a session's port is made of besides its scope.
pub struct PortParts {
    /// The session's working directory.
    pub cwd: PathBuf,
    /// Whether the session may read a path.
    pub may_read: ReadCheck,
    /// A worker's grant.
    pub grant: Option<Grant>,
    /// The session's goal judge (goal-mode workers).
    pub judge: Option<Arc<dyn GoalJudge>>,
}

/// The port of a session of `scope`.
#[must_use]
pub fn port(scope: &ProfileScope, parts: PortParts) -> Arc<dyn KanbanPort> {
    let config = &scope.config().kanban;
    Arc::new(Port {
        profile: scope.name().to_string(),
        current_board: super::project_board(scope, &parts.cwd)
            .or_else(|| config.current_board.clone()),
        limit: config.attachment_max_bytes,
        ttl_ms: super::ttl_ms(config),
        fetcher: super::fetch::fetcher(scope).ok().map(Arc::new),
        scope: scope.clone(),
        parts,
    })
}

struct Port {
    profile: String,
    current_board: Option<String>,
    limit: u64,
    ttl_ms: i64,
    fetcher: Option<Arc<pmt_net::fetch::Fetcher>>,
    scope: ProfileScope,
    parts: PortParts,
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
            let hooks = super::hooks::HookQueue::new(&self.scope);
            let result = match name.as_str() {
                "kanban_attach_url" => self.attach_url(&arguments, &hooks).await,
                "kanban_complete" | "kanban_request_review" => {
                    match self.goal_handoff(&name, &arguments, &hooks).await {
                        Ok(()) => self.run(&name, &arguments, &hooks),
                        Err(err) => Err(err),
                    }
                }
                // Board writes are short SQLite transactions.
                _ => self.run(&name, &arguments, &hooks),
            };
            let scope = self.scope.clone();
            // Lifecycle hooks run after the board work, off the runtime.
            let _ = pmt_core::spawn::blocking(move || hooks.flush(&scope)).await;
            match result {
                Ok(value) => ToolResult::ok(value.to_string()),
                Err(err) => ToolResult::error(err.message().to_owned()),
            }
        })
    }
}

impl Port {
    fn actor(&self) -> Actor {
        match &self.parts.grant {
            Some(g) if g.role == Role::Reviewer => Actor::reviewer(&g.profile),
            Some(g) => Actor::worker(&g.profile, &g.run),
            None => Actor::user(&self.profile),
        }
    }

    fn board(&self, args: &Value, hooks: &Arc<super::hooks::HookQueue>) -> Result<Board, Error> {
        let boards = super::boards(&self.scope, hooks)?;
        let wanted = text(args, "board");
        let slug = match (&self.parts.grant, wanted) {
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
        match (text(args, "task_id"), &self.parts.grant) {
            (Some(id), _) => Ok(id.to_owned()),
            (None, Some(g)) => Ok(g.task.clone()),
            (None, None) => required(args, "task_id").map(str::to_owned),
        }
    }

    /// Refuses a tool outside the session's grant.
    fn permitted(&self, name: &str) -> Result<(), Error> {
        if self.parts.grant.is_some() && NOT_WORKER.contains(&name) {
            return Err(denied(format!(
                "capability denied: {name} is not part of a worker's grant"
            )));
        }
        if self.parts.grant.is_none() && RUN_LIFECYCLE.contains(&name) {
            return Err(denied("not a kanban worker session"));
        }
        Ok(())
    }

    fn run(
        &self,
        name: &str,
        args: &Value,
        hooks: &Arc<super::hooks::HookQueue>,
    ) -> Result<Value, Error> {
        self.permitted(name)?;
        let mut board = self.board(args, hooks)?;
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
            .parts
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
                    metadata: args.get("metadata").filter(|m| m.is_object()).cloned(),
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
                let handoff = Handoff {
                    summary: text(args, "summary").map(str::to_owned),
                    metadata: args.get("metadata").filter(|m| m.is_object()).cloned(),
                    reviewer: None,
                };
                board.request_review(&actor, &id, &handoff, now)?;
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
        new.completion_contract = text(args, "completion_contract").map(str::to_owned);
        new.skills = strings(args, "skills");
        new.model = text(args, "model").map(str::to_owned);
        new.provider = text(args, "provider").map(str::to_owned);
        new.goal_mode = args
            .get("goal_mode")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let created = board.create(
            &self.actor(),
            &new,
            &super::assignees::Assignees::of(&self.scope),
            now,
        )?;
        Ok(serde_json::to_value(created).unwrap_or_default())
    }

    /// A goal-mode worker's handoff: the judge must not say `continue`
    /// (refused) or `unachievable` (the task is blocked); a judge that
    /// cannot be asked lets it through with a WARN.
    async fn goal_handoff(
        &self,
        name: &str,
        args: &Value,
        hooks: &Arc<super::hooks::HookQueue>,
    ) -> Result<(), Error> {
        let (Some(grant), Some(judge)) = (&self.parts.grant, &self.parts.judge) else {
            return Ok(());
        };
        if grant.role != Role::Worker {
            return Ok(());
        }
        let task = self.board(args, hooks)?.task(&grant.task)?;
        if !task.goal_mode {
            return Ok(());
        }
        let evidence = [text(args, "summary"), text(args, "result")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
        let judged = super::goal::verdict(judge.as_ref(), &task, &evidence).await;
        let mut board = self.board(args, hooks)?;
        let actor = self.actor();
        let now = pmt_kanban::now_ms();
        match judged {
            super::goal::Judged::Done(_) => Ok(()),
            super::goal::Judged::Unreachable(err) => {
                tracing::warn!(
                    component = "kanban",
                    task = task.id.as_str(),
                    error = err.as_str(),
                    "goal judge unreachable; allowing lifecycle handoff"
                );
                Ok(())
            }
            super::goal::Judged::Continue(reason) => {
                board.goal_verdict(
                    &actor,
                    &task.id,
                    (0, task.goal_turns()),
                    "continue",
                    &reason,
                    now,
                )?;
                Err(denied(format!(
                    "{name} refused: the goal judge says continue: {reason}"
                )))
            }
            super::goal::Judged::Unachievable(reason) => {
                board.goal_verdict(
                    &actor,
                    &task.id,
                    (0, task.goal_turns()),
                    "unachievable",
                    &reason,
                    now,
                )?;
                board.block(&actor, &task.id, BlockKind::NeedsInput, &reason, now)?;
                Err(denied(format!(
                    "{name} refused: the goal judge says unachievable: {reason}"
                )))
            }
        }
    }

    /// `kanban_attach_url`: fetches `url` under the egress policy, capped
    /// at the attachment limit, and attaches it.
    async fn attach_url(
        &self,
        args: &Value,
        hooks: &Arc<super::hooks::HookQueue>,
    ) -> Result<Value, Error> {
        self.permitted("kanban_attach_url")?;
        let id = self.task_id(args)?;
        if let Some(grant) = &self.parts.grant
            && grant.task != id
        {
            return Err(denied(format!(
                "capability denied: a worker attaches only to its own task {}",
                grant.task
            )));
        }
        let url = required(args, "url")?;
        let fetcher = self
            .fetcher
            .clone()
            .ok_or_else(|| denied("this profile cannot build an outbound client"))?;
        let download = super::fetch::get(&fetcher, url, self.limit).await?;
        let name = text(args, "filename")
            .map(str::to_owned)
            .unwrap_or(download.filename);
        let content_type = text(args, "content_type")
            .map(str::to_owned)
            .or(download.content_type);
        let upload = pmt_kanban::attach::Upload {
            name: &name,
            content_type: content_type.as_deref(),
            bytes: &download.bytes,
        };
        let mut board = self.board(args, hooks)?;
        let attachment = board.attach(
            &self.actor(),
            &id,
            &upload,
            self.limit,
            pmt_kanban::now_ms(),
        )?;
        Ok(serde_json::to_value(attachment).unwrap_or_default())
    }

    fn attach(&self, board: &mut Board, args: &Value, now: i64) -> Result<Value, Error> {
        let id = self.task_id(args)?;
        if let Some(grant) = &self.parts.grant
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
            self.parts.cwd.join(raw)
        };
        if !(self.parts.may_read)(&path) {
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
