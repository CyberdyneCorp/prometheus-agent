//! Lifecycle verbs over several task ids (spec: kanban, CLI surface):
//! each id is tried on its own, reported `ok <id> <status>` or
//! `failed <id>: <reason>` (`not found` for an unknown id), and the
//! command exits 1 when any failed.

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_kanban::{Actor, BlockKind, Board, Completion, Status};
use serde_json::{Value, json};

use super::{Env, KanbanCommand};
use crate::output::{document, print_raw};

/// What one id came to.
type Outcome = Result<Option<Status>, Error>;

fn parse_instant(text: &str) -> Result<i64, Error> {
    text.parse::<jiff::Timestamp>()
        .map(jiff::Timestamp::as_millisecond)
        .map_err(|e| {
            Error::new(
                &codes::KANBAN_INVALID_TASK,
                format!("--at must be an RFC 3339 instant: {e}"),
            )
        })
}

/// Runs a lifecycle verb on each of its ids.
pub(super) fn run(env: &Env<'_>, command: &KanbanCommand) -> Result<u8, Error> {
    let mut board = env.board()?;
    let actor = env.actor();
    let (ids, results) = match command {
        KanbanCommand::Assign { profile, ids } => {
            let profile = (profile != "none").then_some(profile.as_str());
            if let Some(p) = profile
                && !crate::kanban_host::profile_exists(&env.home, p)
            {
                return Err(Error::new(
                    &codes::KANBAN_UNKNOWN_PROFILE,
                    format!("unknown profile '{p}'"),
                ));
            }
            (
                ids,
                each(ids, |id| {
                    board.assign(&actor, id, profile, now()).map(|()| None)
                }),
            )
        }
        KanbanCommand::Block { ids, kind, reason } => {
            let kind = BlockKind::parse(kind)?;
            (
                ids,
                each(ids, |id| {
                    board.block(&actor, id, kind, reason, now()).map(Some)
                }),
            )
        }
        KanbanCommand::Unblock { ids } => (
            ids,
            each(ids, |id| board.unblock(&actor, id, now()).map(Some)),
        ),
        KanbanCommand::Schedule { ids, at } => {
            let wake = at.as_deref().map(parse_instant).transpose()?;
            (
                ids,
                each(ids, |id| {
                    board
                        .schedule(&actor, id, wake, now())
                        .map(|()| Some(Status::Scheduled))
                }),
            )
        }
        KanbanCommand::Complete {
            ids,
            result,
            summary,
            run,
            force,
        } => {
            let completion = Completion {
                result: result.clone(),
                summary: summary.clone(),
                run_id: run.clone(),
                force: *force,
                attachment_limit: env.scope.config().kanban.attachment_max_bytes,
                ..Completion::default()
            };
            (
                ids,
                each(ids, |id| {
                    board.complete(&actor, id, &completion, now()).map(Some)
                }),
            )
        }
        other => review(env, &mut board, &actor, other)?,
    };
    report(env, ids, &results)
}

/// The review, archive and promotion verbs.
fn review<'a>(
    _env: &Env<'_>,
    board: &mut Board,
    actor: &Actor,
    command: &'a KanbanCommand,
) -> Result<(&'a Vec<String>, Vec<Outcome>), Error> {
    Ok(match command {
        KanbanCommand::RequestReview { ids, summary } => (
            ids,
            each(ids, |id| {
                board
                    .request_review(actor, id, summary.as_deref(), now())
                    .map(|()| Some(Status::Review))
            }),
        ),
        KanbanCommand::Approve { ids, comment } => (
            ids,
            each(ids, |id| {
                board
                    .approve(actor, id, comment.as_deref(), now())
                    .map(|()| Some(Status::Done))
            }),
        ),
        KanbanCommand::RequestChanges { ids, comment } => (
            ids,
            each(ids, |id| {
                board
                    .request_changes(actor, id, comment, now())
                    .map(|()| Some(Status::Todo))
            }),
        ),
        KanbanCommand::Archive { ids } => (
            ids,
            each(ids, |id| {
                board
                    .archive(actor, id, now())
                    .map(|()| Some(Status::Archived))
            }),
        ),
        KanbanCommand::Restore { ids, to } => {
            let to = to.as_deref().map(Status::parse).transpose()?;
            (
                ids,
                each(ids, |id| board.restore(actor, id, to, now()).map(Some)),
            )
        }
        KanbanCommand::Promote { ids } => (ids, promote(board, ids)?),
        _ => unreachable!("kanban_cmd::run dispatches every other verb"),
    })
}

/// `kanban promote`: the promotion pass, reporting the named ids (or
/// every promoted one when none is named).
fn promote(board: &mut Board, ids: &[String]) -> Result<Vec<Outcome>, Error> {
    let promoted = board.promote_all(&Actor::user("cli"), now())?;
    Ok(ids
        .iter()
        .map(|id| {
            let task = board.task(id)?;
            if promoted.contains(id) || task.status == Status::Ready {
                Ok(Some(task.status))
            } else {
                Err(Error::new(
                    &codes::KANBAN_ILLEGAL_TRANSITION,
                    format!("{id} stays {} (open parents or not in todo)", task.status),
                ))
            }
        })
        .collect())
}

fn now() -> i64 {
    pmt_kanban::now_ms()
}

fn each(ids: &[String], mut op: impl FnMut(&str) -> Outcome) -> Vec<Outcome> {
    ids.iter().map(|id| op(id)).collect()
}

fn reason(err: &Error) -> String {
    if err.code().code == codes::KANBAN_TASK_NOT_FOUND.code {
        "not found".to_owned()
    } else {
        err.message().to_owned()
    }
}

fn report(env: &Env<'_>, ids: &[String], results: &[Outcome]) -> Result<u8, Error> {
    let failed = results.iter().any(Result::is_err);
    let code = u8::from(failed);
    if env.ctx.json {
        let rows: Vec<Value> = ids
            .iter()
            .zip(results)
            .map(|(id, r)| match r {
                Ok(status) => json!({"id": id, "ok": true, "status": status}),
                Err(err) => {
                    json!({"id": id, "ok": false, "error": reason(err), "code": err.code().code})
                }
            })
            .collect();
        document("kanban results", &json!({"results": rows}))?;
        return Ok(code);
    }
    let mut out = String::new();
    for (id, r) in ids.iter().zip(results) {
        let _ = match r {
            Ok(Some(status)) => writeln!(out, "ok {id} {status}"),
            Ok(None) => writeln!(out, "ok {id}"),
            Err(err) => writeln!(out, "failed {id}: {}", reason(err)),
        };
    }
    print_raw(&out)?;
    Ok(code)
}
