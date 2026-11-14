//! The worker's first message (spec: kanban, Worker spawn by run ticket;
//! the `context` verb prints exactly this): the task, its parents'
//! results, its comments and its attachments.

use std::fmt::Write as _;

use pmt_core::error::Error;

use crate::board::Board;

/// The kanban worker guidance every worker session's frozen system prompt
/// carries.
pub const WORKER_GUIDANCE: &str = "## Kanban worker\n\
You are working one task of a shared kanban board. The user message holds the task, \
its parents' results, comments and attachments. Work in the current directory (the \
task's workspace). Finish with exactly one of: kanban_complete (with a result or \
summary that shows the evidence), kanban_request_review, or kanban_block (with a kind \
and a reason). Use kanban_heartbeat with a short note during long work, kanban_comment \
for progress worth keeping, and kanban_create for follow-up tasks (list the ids you \
created in kanban_complete's created_tasks). Never claim work you did not do.";

/// The context text of task `id`.
///
/// # Errors
///
/// `kanban.task_not_found`, store errors.
pub fn render(board: &Board, id: &str) -> Result<String, Error> {
    let task = board.task(id)?;
    let mut out = format!(
        "# Kanban task {} on board {}\n\n## {}\n\n",
        task.id,
        board.slug(),
        task.title
    );
    if !task.body.trim().is_empty() {
        let _ = writeln!(out, "{}\n", task.body.trim());
    }
    let _ = writeln!(out, "Status: {}. Kind: {}.", task.status, task.kind.name());
    if !task.artifacts.is_empty() {
        let _ = writeln!(
            out,
            "Declared artifacts (leave them in the workspace): {}.",
            task.artifacts.join(", ")
        );
    }
    let parents = board.parents(id)?;
    if !parents.is_empty() {
        out.push_str("\n## Parent results\n");
        for parent in parents {
            let p = board.task(&parent)?;
            let result = p
                .result
                .or(p.summary)
                .unwrap_or_else(|| "(no result)".to_owned());
            let _ = writeln!(
                out,
                "- {} \"{}\" ({}): {}",
                p.id,
                p.title,
                p.status,
                result.trim()
            );
        }
    }
    let comments = board.comments(id)?;
    if !comments.is_empty() {
        out.push_str("\n## Comments\n");
        for c in comments {
            let _ = writeln!(out, "- {}: {}", c.author, c.body.trim());
        }
    }
    let attachments = board.attachments(id)?;
    if !attachments.is_empty() {
        out.push_str("\n## Attachments\n");
        for a in attachments {
            let _ = writeln!(
                out,
                "- {} ({}, {} bytes): {}",
                a.filename,
                a.content_type,
                a.size,
                board.attachment_path(&a).display()
            );
        }
    }
    Ok(out)
}
