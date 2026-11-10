//! `prometheus sessions fork|siblings|import` (spec: session-storage,
//! Session forking API; Portable export and import).

use std::fmt::Write as _;
use std::path::Path;

use pmt_core::error::Error;
use pmt_core::id::{MessageId, SessionId};
use pmt_store::Store;
use serde_json::json;

use crate::context::Context;
use crate::output::{json as print_json, print_raw};
use crate::session_export::session_json;

/// `sessions fork`.
pub(super) fn fork(
    ctx: &Context<'_>,
    store: &Store,
    id: SessionId,
    at: Option<&str>,
    title: Option<&str>,
) -> Result<u8, Error> {
    let at = at.map(str::parse::<MessageId>).transpose()?;
    let fork = store.fork_session(id, at, title)?;
    if ctx.json {
        return print_json(&json!({"session": session_json(&fork)}));
    }
    eprintln!(
        "forked {id} as {}{}",
        fork.id,
        fork.title
            .as_deref()
            .map(|t| format!(" ({t})"))
            .unwrap_or_default()
    );
    print_raw(&format!("{}\n", fork.id))
}

/// `sessions siblings`.
pub(super) fn siblings(ctx: &Context<'_>, store: &Store, id: SessionId) -> Result<u8, Error> {
    let nodes = store.siblings(id)?;
    if ctx.json {
        let siblings: Vec<_> = nodes
            .iter()
            .map(|n| json!({"id": n.id.to_string(), "parent": n.parent.to_string(), "kind": n.kind.as_str()}))
            .collect();
        return print_json(&json!({"session": id.to_string(), "siblings": siblings}));
    }
    if nodes.is_empty() {
        eprintln!("{id} has no siblings");
        return Ok(0);
    }
    let mut text = String::new();
    for n in &nodes {
        let _ = writeln!(text, "{} ({})", n.id, n.kind);
    }
    print_raw(&text)
}

/// `sessions import`.
pub(super) fn import(
    ctx: &Context<'_>,
    store: &Store,
    file: &Path,
    remap_ids: bool,
) -> Result<u8, Error> {
    let imported = crate::session_import::import(store, file, remap_ids)?;
    if ctx.json {
        return print_json(&imported);
    }
    eprintln!(
        "imported {} session(s), {} message(s), {} blob(s)",
        imported.sessions.len(),
        imported.messages,
        imported.blobs
    );
    Ok(0)
}
