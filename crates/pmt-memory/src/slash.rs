//! `/memory` on every chat surface (spec: memory, Write approval and
//! pending changes; Snapshot refresh only at compression or new session;
//! cli, deferred effects with `--now`).
//!
//! `/memory [status]`, `/memory pending`, `/memory approve <id>|all`,
//! `/memory reject <id>|all` and `/memory reload [--now]`. The slash
//! registry renders [`Reply::text`] and, on [`Reply::rebuild_now`], calls
//! `Agent::rebuild_prompt(Rebuild::Now("memory"))` (`prompt.rebuilt` with
//! reason `memory --now`).

use std::fmt::Write as _;

use pmt_config::schema::MemoryConfig;
use pmt_core::error::Error;
use pmt_store::Store;

use crate::{budget, status, write};

/// The usage line.
pub const USAGE: &str =
    "usage: /memory [status | pending | approve <id>|all | reject <id>|all | reload [--now]]";

/// The reply to `/memory reload` without `--now`.
pub const DEFERRED: &str = "Memory reload in your next session. Use /new to start one now, or add --now to apply immediately (invalidates prompt cache).";

/// The reply to `/memory reload --now`.
pub const APPLIED_NOW: &str =
    "Memory snapshot will be re-read at the next turn (prompt cache invalidated).";

/// What `/memory` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The text shown.
    pub text: String,
    /// Rebuild the system prompt at the next turn.
    pub rebuild_now: bool,
}

fn plain(text: impl Into<String>) -> Reply {
    Reply {
        text: text.into(),
        rebuild_now: false,
    }
}

/// Runs `/memory <args>`.
///
/// # Errors
///
/// Store errors; a refused approval (`memory.stale`,
/// `memory.pending_not_found`...).
pub fn run(store: &Store, config: &MemoryConfig, args: &str) -> Result<Reply, Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] | ["status"] => Ok(plain(status::render(&status::status(store, config)?))),
        ["pending"] => pending(store),
        ["approve", id] => resolve(store, config, id, true),
        ["reject", id] => resolve(store, config, id, false),
        ["reload"] => Ok(plain(DEFERRED)),
        ["reload", "--now"] => Ok(Reply {
            text: APPLIED_NOW.to_owned(),
            rebuild_now: true,
        }),
        _ => Ok(plain(USAGE)),
    }
}

fn pending(store: &Store) -> Result<Reply, Error> {
    let pending = store.memory_pending()?;
    if pending.is_empty() {
        return Ok(plain("No pending memory changes."));
    }
    let mut text = String::new();
    for p in &pending {
        let _ = writeln!(text, "{} ({}, {}):", p.id, p.target, p.origin);
        for line in write::describe(p) {
            let _ = writeln!(text, "  {line}");
        }
    }
    text.push_str("Approve with /memory approve <id>|all, reject with /memory reject <id>|all.");
    Ok(plain(text))
}

fn resolve(
    store: &Store,
    config: &MemoryConfig,
    which: &str,
    approve: bool,
) -> Result<Reply, Error> {
    let ids: Vec<String> = if which == "all" {
        store.memory_pending()?.into_iter().map(|p| p.id).collect()
    } else {
        vec![which.to_owned()]
    };
    if ids.is_empty() {
        return Ok(plain("No pending memory changes."));
    }
    let mut text = String::new();
    for id in &ids {
        if approve {
            let written = write::approve(store, config, id)?;
            let _ = writeln!(
                text,
                "Approved {id}: {} change(s) applied, core usage {}.",
                written.results.len(),
                written.usage.indicator()
            );
        } else {
            write::reject(store, id)?;
            let _ = writeln!(text, "Rejected {id}.");
        }
    }
    if approve && budget::enabled(config).is_empty() {
        text.push_str(
            "Built-in memory is disabled; the entries are kept but not shown to the model.\n",
        );
    }
    Ok(plain(text.trim_end().to_owned()))
}
