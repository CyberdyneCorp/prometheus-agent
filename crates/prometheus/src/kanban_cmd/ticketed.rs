//! `prometheus kanban <verb> --ticket <line|->` (spec: kanban, Worker
//! lanes; Additional CLI verbs): an external worker — a plugin's worker
//! lane, or a person's manual `claim` — reports as the run its ticket
//! names. The ticket's secret must match the run's open claim; it is not
//! consumed, so the worker can heartbeat, comment and attach before its
//! one terminator. This mode runs before anything touches the home (no
//! profile, no logs). A lane's sandbox cannot write the board at all: its
//! report goes through its mailbox ([`relay`]) to the lane's broker, which
//! answers it with [`answer`] outside the sandbox.

use std::io::BufRead as _;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_kanban::attach::Upload;
use pmt_kanban::ticket::Ticket;
use pmt_kanban::{Actor, BlockKind, Board, Boards, Completion, Handoff, NewTask, Status};
use serde_json::{Value, json};

use super::{KanbanArgs, KanbanCommand};

/// The attachment cap of a manual ticketed worker (run before the home's
/// configuration is read: the default).
const ATTACHMENT_MAX: u64 = 25 * 1024 * 1024;

/// The ticket line: `text`, or stdin's first line for `-`.
fn ticket_line(text: &str) -> String {
    if text != "-" {
        return text.to_owned();
    }
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    line
}

/// Where `kanban attach`'s bytes come from, and the attachment cap.
pub enum Files<'a> {
    /// The named paths (a person's manual claim).
    Local(u64),
    /// Copies a lane staged in its mailbox, in order: the broker never
    /// opens a path the lane names.
    Staged {
        /// The mailbox.
        dir: &'a Path,
        /// The staged file names.
        names: &'a [String],
        /// The cap.
        limit: u64,
    },
}

impl Files<'_> {
    fn limit(&self) -> u64 {
        match self {
            Self::Local(limit) | Self::Staged { limit, .. } => *limit,
        }
    }

    /// The display name and bytes of the `n`-th file, `path` as given.
    fn read(&self, n: usize, path: &Path) -> Result<(String, Vec<u8>), Error> {
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let source = match self {
            Self::Local(_) => path.to_path_buf(),
            Self::Staged { dir, names, .. } => names
                .get(n)
                .map(|staged| dir.join(staged))
                .ok_or_else(|| denied("a file to attach was not staged"))?,
        };
        Ok((
            name,
            pmt_kanban::attach::read_limited(&source, self.limit())?,
        ))
    }
}

fn denied(message: impl Into<String>) -> Error {
    Error::new(&codes::KANBAN_NOT_PERMITTED, message)
}

/// Runs `args` (which carry `--ticket`) for the home at `home`.
///
/// # Errors
///
/// As [`answer`].
pub fn run(args: &KanbanArgs, home: &Path, json: bool) -> Result<u8, Error> {
    let ticket = Ticket::read(&ticket_line(args.ticket.as_deref().unwrap_or("-")))?;
    print(
        &answer(args, home, &ticket, &Files::Local(ATTACHMENT_MAX))?,
        json,
    )
}

/// A lane's report: sends `args` (this process's command line) with its
/// ticket and staged files to the lane's broker through `mailbox`, and
/// prints the answer.
///
/// # Errors
///
/// The broker's answer's error; the mailbox's.
pub fn relay(args: &KanbanArgs, mailbox: &Path, json: bool) -> Result<u8, Error> {
    let ticket = ticket_line(args.ticket.as_deref().unwrap_or("-"));
    let files: &[PathBuf] = match &args.command {
        KanbanCommand::Attach { files, .. } => files,
        _ => &[],
    };
    let command_line = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let value = crate::kanban_host::mailbox::send(mailbox, command_line, ticket, files)?;
    print(&value, json)
}

/// Applies `args` as the worker of `ticket`'s run on the home at `home`;
/// the command's JSON answer.
///
/// # Errors
///
/// `kanban.ticket_refused` for a ticket that matches no open run,
/// `kanban.not_permitted` for a verb or task outside the run, the
/// operation's own errors.
pub fn answer(
    args: &KanbanArgs,
    home: &Path,
    ticket: &Ticket,
    files: &Files<'_>,
) -> Result<Value, Error> {
    if let Some(flag) = args.board.as_deref().filter(|b| *b != ticket.board) {
        return Err(denied(format!("capability denied: kanban board '{flag}'")));
    }
    // PR completion contracts read GitHub with the assignee profile's own
    // credential (a lane is no profile and has none: its contract
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
        KanbanCommand::Attach { id, files: paths } => {
            own(id)?;
            let mut stored = Vec::new();
            for (n, path) in paths.iter().enumerate() {
                let (name, bytes) = files.read(n, path)?;
                let upload = Upload {
                    name: &name,
                    content_type: None,
                    bytes: &bytes,
                };
                stored.push(board.attach(&actor, id, &upload, files.limit(), now)?);
            }
            json!({"task_id": id, "attachments": stored})
        }
        other => terminate(&mut board, &actor, other, &own, (now, files.limit()))?,
    };
    Ok(out)
}

/// The terminators and `create` of a ticketed worker.
fn terminate(
    board: &mut Board,
    actor: &Actor,
    command: &KanbanCommand,
    own: &dyn Fn(&str) -> Result<(), Error>,
    (now, attachment_limit): (i64, u64),
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
                attachment_limit,
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
