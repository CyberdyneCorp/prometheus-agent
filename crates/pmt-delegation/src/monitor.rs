//! The agents monitor's view of a session's children (spec:
//! subagent-delegation, Agents monitor and live transcripts): its running
//! and recently finished subagents as a tree grouped by parent, and the
//! per-item actions `/agents` offers (stop, steer, tail). Every action
//! reaches only the session's own spawn tree; stopping one child never
//! touches its siblings.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use pmt_agent::turn::Issuer;

use crate::Refusal;
use crate::tree::{Entry, Registry, State};

/// Lines of a transcript `/agents tail` shows.
pub const TAIL_LINES: usize = 20;

/// The subagent tree of `conversation`, one line per child, indented
/// under its parent; empty when it has none.
#[must_use]
pub fn tree(conversation: &str) -> Vec<String> {
    let entries = Registry::process().tree(conversation);
    let sessions: Vec<String> = entries.iter().filter_map(|e| e.session()).collect();
    let mut lines = Vec::new();
    for top in entries
        .iter()
        .filter(|e| !sessions.contains(&e.parent_session))
    {
        push(&entries, top, 1, &mut lines);
    }
    lines
}

fn push(entries: &[Arc<Entry>], entry: &Entry, depth: usize, out: &mut Vec<String>) {
    out.push(line(entry, depth));
    let Some(session) = entry.session() else {
        return;
    };
    for child in entries.iter().filter(|e| e.parent_session == session) {
        push(entries, child, depth + 1, out);
    }
}

/// One child: id, status, elapsed time, model, tokens, cost and goal.
fn line(entry: &Entry, depth: usize) -> String {
    let snapshot = entry.snapshot();
    let tokens = &snapshot.usage.tokens;
    let total = tokens.input + tokens.output + tokens.cache_read + tokens.cache_write;
    let mut text = format!(
        "{}{} {} {}s {} {} tokens",
        "  ".repeat(depth),
        entry.subagent_id,
        entry.state().name(),
        entry.elapsed().as_secs(),
        if entry.model().is_empty() {
            "-".to_owned()
        } else {
            entry.model()
        },
        total,
    );
    if let Some(cost) = snapshot.usage.cost_usd {
        let _ = write!(text, " ${cost:.4}");
    }
    let _ = write!(
        text,
        " — {}",
        pmt_core::subagent_tree::one_line(&entry.goal)
    );
    text
}

/// Stops child `id` of `conversation`'s tree (it ends `interrupted`); the
/// confirmation.
///
/// # Errors
///
/// The not-in-tree refusal; an ended child cannot be stopped.
pub fn stop(conversation: &str, id: &str) -> Result<String, Refusal> {
    let entry = open_entry(conversation, id)?;
    entry.request_stop(Issuer::User);
    Ok(format!("Stopping {id}; its siblings keep running."))
}

/// Steers child `id` of `conversation`'s tree with `message`.
///
/// # Errors
///
/// The not-in-tree refusal, or an empty message.
pub fn steer(conversation: &str, id: &str, message: &str) -> Result<String, Refusal> {
    let entry = open_entry(conversation, id)?;
    crate::host::steer_entry(&entry, Some(message))?;
    Ok(format!("Steer queued for {id}."))
}

/// The last [`TAIL_LINES`] lines of child `id`'s live transcript.
///
/// # Errors
///
/// The not-in-tree refusal.
pub fn tail(conversation: &str, id: &str) -> Result<String, Refusal> {
    let entry = Registry::process().in_tree(conversation, id)?;
    let Some(path) = entry.transcript() else {
        return Ok(format!("{id} has no live transcript."));
    };
    Ok(last_lines(path, TAIL_LINES))
}

/// The child's session once it returned (inspect shows its history).
///
/// # Errors
///
/// The not-in-tree refusal, or a child still running.
pub fn returned_session(conversation: &str, id: &str) -> Result<String, Refusal> {
    let entry = Registry::process().in_tree(conversation, id)?;
    match (entry.state(), entry.session()) {
        (State::Ended(_), Some(session)) => Ok(session),
        (State::Ended(_), None) => Err(Refusal::new(
            &pmt_core::error::codes::DELEGATION_NOT_IN_TREE,
            format!("{id} ended before its session was built."),
        )),
        _ => Err(Refusal::new(
            &pmt_core::error::codes::DELEGATION_INVALID_TASK,
            format!(
                "{id} is still running; inspect shows its history once it returns (tail follows it live)."
            ),
        )),
    }
}

fn open_entry(conversation: &str, id: &str) -> Result<Arc<Entry>, Refusal> {
    let entry = Registry::process().in_tree(conversation, id)?;
    if matches!(entry.state(), State::Ended(_)) {
        return Err(Refusal::new(
            &pmt_core::error::codes::DELEGATION_INVALID_TASK,
            format!("{id} has already finished."),
        ));
    }
    Ok(entry)
}

fn last_lines(path: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
