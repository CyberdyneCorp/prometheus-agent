//! `prometheus kanban <verb> --ticket <line|->` (spec: kanban, Worker
//! lanes; Additional CLI verbs): an external worker — a plugin's worker
//! lane, or a person's manual `claim` — reports as the run its ticket
//! names. The ticket's secret must match the run's open claim; it is not
//! consumed, so the worker can heartbeat, comment and attach before its
//! one terminator. This mode runs before anything touches the home (no
//! profile, no logs): a lane's sandbox reaches only its own board and the
//! blob store.

use std::io::BufRead as _;
use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_kanban::ticket::Ticket;
use pmt_kanban::{Actor, BlockKind, Board, Boards, Completion, Handoff, NewTask, Status};
use serde_json::{Value, json};

use super::{KanbanArgs, KanbanCommand};

/// The attachment cap of a ticketed worker (the home's configuration is
/// out of its reach: the default).
const ATTACHMENT_MAX: u64 = 25 * 1024 * 1024;

fn ticket(text: &str) -> Result<Ticket, Error> {
    if text != "-" {
        return Ticket::read(text);
    }
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    Ticket::read(&line)
}

fn denied(message: impl Into<String>) -> Error {
    Error::new(&codes::KANBAN_NOT_PERMITTED, message)
}

/// Runs `args` (which carry `--ticket`) for the home at `home`.
///
/// # Errors
///
/// `kanban.ticket_refused` for a ticket that matches no open run,
/// `kanban.not_permitted` for a verb or task outside the run, the
/// operation's own errors.
pub fn run(args: &KanbanArgs, home: &Path, json: bool) -> Result<u8, Error> {
    let ticket = ticket(args.ticket.as_deref().unwrap_or("-"))?;
    if let Some(flag) = args.board.as_deref().filter(|b| *b != ticket.board) {
        return Err(denied(format!("capability denied: kanban board '{flag}'")));
    }
    // PR completion contracts read GitHub with the assignee profile's own
    // credential (a sandboxed lane cannot reach it: its contract
    // completions are refused as `auth`).
    let boards = Boards::existing(home).with_pr_checks(std::sync::Arc::new(
        crate::kanban_host::github::GitHub::new(home),
    ));
    let mut board = boards.board(&ticket.board)?;
    let run = board.authenticate(&ticket.run, &ticket.secret)?;
    let actor = Actor::worker(&run.profile, &run.id);
    let own = |id: &str| -> Result<(), Error> {
        if id == run.task_id {
            Ok(())
        } else {
            Err(denied(format!(
                "capability denied: run {} works task {}, not {id}",
                run.id, run.task_id
            )))
        }
    };
    let now = pmt_kanban::now_ms();
    let out: Value = match &args.command {
        KanbanCommand::Show { id } => crate::kanban_host::tool::show(&board, id)?,
        KanbanCommand::Context { id } => {
            json!({"context": pmt_kanban::context::render(&board, id)?})
        }
        KanbanCommand::Comment { id, body } => {
            board.comment(&actor, id, body, now)?;
            json!({"task_id": id, "commented": true})
        }
        KanbanCommand::Heartbeat { id, note } => {
            own(id)?;
            board.heartbeat(&actor, &run.id, note.as_deref(), 900_000, now)?;
            json!({"task_id": id, "renewed": true})
        }
        KanbanCommand::Attach { id, files } => {
            own(id)?;
            let mut stored = Vec::new();
            for file in files {
                stored.push(board.attach_file(&actor, id, file, ATTACHMENT_MAX, now)?);
            }
            json!({"task_id": id, "attachments": stored})
        }
        other => terminate(&mut board, &actor, other, &own, now)?,
    };
    print(&out, json)
}

/// The terminators and `create` of a ticketed worker.
fn terminate(
    board: &mut Board,
    actor: &Actor,
    command: &KanbanCommand,
    own: &dyn Fn(&str) -> Result<(), Error>,
    now: i64,
) -> Result<Value, Error> {
    let one = |ids: &[String]| -> Result<String, Error> {
        match ids {
            [id] => own(id).map(|()| id.clone()),
            _ => Err(denied("a ticketed worker acts on its own task only")),
        }
    };
    Ok(match command {
        KanbanCommand::Complete {
            ids,
            result,
            summary,
            metadata,
            ..
        } => {
            let id = one(ids)?;
            let completion = Completion {
                result: result.clone(),
                summary: summary.clone(),
                metadata: metadata
                    .as_deref()
                    .map(super::lifecycle::parse_metadata)
                    .transpose()?,
                attachment_limit: ATTACHMENT_MAX,
                ..Completion::default()
            };
            json!({"task_id": id, "status": board.complete(actor, &id, &completion, now)?})
        }
        KanbanCommand::Block { ids, kind, reason } => {
            let id = one(ids)?;
            json!({"task_id": id, "status": board.block(actor, &id, BlockKind::parse(kind)?, reason, now)?})
        }
        KanbanCommand::RequestReview {
            ids,
            summary,
            metadata,
            ..
        } => {
            let id = one(ids)?;
            let handoff = Handoff {
                summary: summary.clone(),
                metadata: metadata
                    .as_deref()
                    .map(super::lifecycle::parse_metadata)
                    .transpose()?,
                reviewer: None,
            };
            board.request_review(actor, &id, &handoff, now)?;
            json!({"task_id": id, "status": Status::Review})
        }
        KanbanCommand::Create(create) => {
            let mut new = NewTask::titled(&create.title);
            new.body = create.body.clone().unwrap_or_default();
            new.parents.clone_from(&create.parents);
            let home = board
                .dir()
                .ancestors()
                .nth(3)
                .map(Path::to_path_buf)
                .unwrap_or_default();
            new.assignee.clone_from(&create.assignee);
            let created = board.create(
                actor,
                &new,
                &crate::kanban_host::assignees::Assignees::home(&home),
                now,
            )?;
            serde_json::to_value(created).unwrap_or_default()
        }
        _ => {
            return Err(denied(
                "with --ticket only show, context, comment, heartbeat, attach, create, complete, block and request-review run",
            ));
        }
    })
}

fn print(value: &Value, json: bool) -> Result<u8, Error> {
    if json {
        return crate::output::json(value);
    }
    match value.get("status").and_then(Value::as_str) {
        Some(status) => crate::output::print(&format!(
            "ok {} {status}",
            value["task_id"].as_str().unwrap_or("-")
        )),
        None => crate::output::json(value),
    }
}
