//! `/memory` on every chat surface (spec: memory, Write approval and
//! pending changes; Snapshot refresh only at compression or new session;
//! cli, deferred effects with `--now`).
//!
//! `/memory [status]`, `/memory pending`, `/memory approve <id>|all`,
//! `/memory reject <id>|all`, `/memory review [keep|revert <rev>|all]`
//! (spec: Reviewable memory changes per session) and `/memory reload
//! [--now]`. The slash
//! registry renders [`Reply::text`] and, on [`Reply::rebuild_now`], calls
//! `Agent::rebuild_prompt(Rebuild::Now("memory"))` (`prompt.rebuilt` with
//! reason `memory --now`).

use std::fmt::Write as _;

use pmt_config::schema::MemoryConfig;
use pmt_core::error::Error;
use pmt_core::text::Catalog;
use pmt_store::Store;

use crate::{budget, status, write};

/// The usage line.
pub const USAGE: &str = "usage: /memory [status | pending | approve <id>|all | reject <id>|all | review [keep|revert <rev>|all] | reload [--now]]";

/// The catalog key of the reply to `/memory reload` without `--now`.
pub const DEFERRED: &str = "slash.memory.deferred";

/// The catalog key of the reply to `/memory reload --now`.
pub const APPLIED_NOW: &str = "slash.memory.applied_now";

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

/// Runs `/memory <args>` in `session` (`review` needs one), worded with
/// `text`.
///
/// # Errors
///
/// Store errors; a refused approval (`memory.stale`,
/// `memory.pending_not_found`...).
pub fn run(
    text: &dyn Catalog,
    store: &Store,
    config: &MemoryConfig,
    session: Option<&str>,
    args: &str,
) -> Result<Reply, Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        ["review", rest @ ..] => match session {
            Some(session) => Ok(plain(crate::journal::review(text, store, session, rest)?)),
            None => Ok(plain(text.text("slash.memory.review_needs_session", &[]))),
        },
        [] | ["status"] => Ok(plain(status::render(text, &status::status(store, config)?))),
        ["pending"] => pending(text, store),
        ["approve", id] => resolve(text, store, config, id, true),
        ["reject", id] => resolve(text, store, config, id, false),
        ["reload"] => Ok(plain(text.text(DEFERRED, &[]))),
        ["reload", "--now"] => Ok(Reply {
            text: text.text(APPLIED_NOW, &[]),
            rebuild_now: true,
        }),
        _ => Ok(plain(USAGE)),
    }
}

fn pending(catalog: &dyn Catalog, store: &Store) -> Result<Reply, Error> {
    let pending = store.memory_pending()?;
    if pending.is_empty() {
        return Ok(plain(catalog.text("slash.memory.no_pending", &[])));
    }
    let mut text = String::new();
    for p in &pending {
        let _ = writeln!(text, "{} ({}, {}):", p.id, p.target, p.origin);
        for line in write::describe(p) {
            let _ = writeln!(text, "  {line}");
        }
    }
    text.push_str(&catalog.text("slash.memory.pending_hint", &[]));
    Ok(plain(text))
}

fn resolve(
    catalog: &dyn Catalog,
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
        return Ok(plain(catalog.text("slash.memory.no_pending", &[])));
    }
    let mut text = String::new();
    for id in &ids {
        if approve {
            let written = write::approve(store, config, id)?;
            let line = catalog.text(
                "slash.memory.approved",
                &[
                    ("id", id),
                    ("count", &written.results.len()),
                    ("usage", &written.usage.indicator()),
                ],
            );
            let _ = writeln!(text, "{line}");
        } else {
            write::reject(store, id)?;
            let _ = writeln!(
                text,
                "{}",
                catalog.text("slash.memory.rejected", &[("id", id)])
            );
        }
    }
    if approve && budget::enabled(config).is_empty() {
        let _ = writeln!(text, "{}", catalog.text("slash.memory.disabled_kept", &[]));
    }
    Ok(plain(text.trim_end().to_owned()))
}
