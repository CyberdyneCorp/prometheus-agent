//! Committing and staging batches, and approving or rejecting staged ones
//! (spec: memory, Atomic batches judged on the final state; Write approval
//! and pending changes; Background review cannot delete unattended).
//!
//! A call's committed and staged operations go through one store
//! transaction: the committed ones are applied, the staged ones are
//! applied to a throwaway copy of that result (so a proposal is validated
//! and its selectors resolved to ids) and recorded with the content of
//! every entry they touch, which [`approve`] requires to be unchanged.

use std::collections::BTreeMap;

use jiff::Timestamp;
use pmt_config::schema::MemoryConfig;
use pmt_core::error::{Error, codes};
use pmt_store::{MemoryPending, MemoryTarget, Store};
use serde::Serialize;
use serde_json::Value;

use crate::batch::{self, Context, OpResult};
use crate::budget::{self, Usage};
use crate::ops::Op;

/// What a write did.
#[derive(Debug, Clone, Serialize)]
pub struct Written {
    /// Results of the committed operations.
    pub results: Vec<OpResult>,
    /// Results of the staged operations (adds have no id yet).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub staged: Vec<Staged>,
    /// The target's usage after the commit.
    pub usage: Usage,
    /// The staged change, when one was recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_id: Option<String>,
}

/// One staged operation.
#[derive(Debug, Clone, Serialize)]
pub struct Staged {
    /// The action.
    pub action: crate::ops::Action,
    /// The entry it changes (not for adds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// The batch context of `target` under `config`.
#[must_use]
pub fn context(
    config: &MemoryConfig,
    target: MemoryTarget,
    origin: pmt_store::MemoryOrigin,
    session: Option<String>,
) -> Context {
    Context {
        target,
        origin,
        session,
        now: Timestamp::now(),
        core_limit: budget::limit(config, target),
        archival_max: usize::try_from(config.archival_max_entries).unwrap_or(usize::MAX),
    }
}

fn refused(message: String) -> Error {
    let code = if message.contains("core tier is at") || message.contains("archival_max_entries") {
        &codes::MEMORY_OVER_BUDGET
    } else {
        &codes::MEMORY_INVALID
    };
    Error::new(code, message)
}

/// Commits `commit` and stages `stage` in one transaction.
///
/// # Errors
///
/// `memory.invalid` / `memory.over_budget` with the refusal text (nothing
/// written); store errors.
pub fn write(store: &Store, ctx: &Context, commit: &[Op], stage: &[Op]) -> Result<Written, Error> {
    store.memory_batch(Some(ctx.target), |b| {
        let mut entries = b.entries.clone();
        let applied = batch::apply(&mut entries, commit, ctx).map_err(refused)?;
        let mut written = Written {
            results: applied.results,
            staged: Vec::new(),
            usage: applied.usage,
            pending_id: None,
        };
        if !stage.is_empty() {
            let mut preview = entries.clone();
            let proposal = batch::apply(&mut preview, stage, ctx).map_err(refused)?;
            written.staged = proposal
                .resolved
                .iter()
                .map(|op| Staged {
                    action: op.action,
                    id: op.id.clone(),
                })
                .collect();
            if !proposal.resolved.is_empty() {
                let id = crate::id::pending();
                b.stage.push(MemoryPending {
                    id: id.clone(),
                    target: ctx.target,
                    operations: serde_json::to_value(&proposal.resolved).unwrap_or_default(),
                    base: serde_json::to_value(&proposal.base).unwrap_or_default(),
                    origin: ctx.origin,
                    session_id: ctx.session.clone(),
                    created_at: ctx.now,
                });
                written.pending_id = Some(id);
            }
        }
        b.entries = entries;
        Ok(written)
    })
}

fn not_found(id: &str) -> Error {
    Error::new(
        &codes::MEMORY_PENDING_NOT_FOUND,
        format!("no pending memory change '{id}'"),
    )
    .with_hint("prometheus memory pending")
}

fn operations(pending: &MemoryPending) -> Result<Vec<Op>, Error> {
    serde_json::from_value(pending.operations.clone()).map_err(|e| {
        Error::new(
            &codes::MEMORY_INVALID,
            format!("pending change '{}' is unreadable: {e}", pending.id),
        )
    })
}

/// Applies staged change `id` unless an entry it was proposed against has
/// changed since (`memory.stale`, store untouched).
///
/// # Errors
///
/// `memory.pending_not_found`, `memory.stale`, `memory.over_budget`,
/// `memory.invalid`; store errors.
pub fn approve(store: &Store, config: &MemoryConfig, id: &str) -> Result<Written, Error> {
    let target = store
        .memory_pending()?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| not_found(id))?
        .target;
    store.memory_batch(Some(target), |b| {
        let pending = b
            .pending
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| not_found(id))?;
        let base: BTreeMap<String, String> =
            serde_json::from_value(pending.base.clone()).unwrap_or_default();
        for (entry, content) in &base {
            if !b.entries.iter().any(|e| e.id == *entry && e.content == *content) {
                return Err(Error::new(
                    &codes::MEMORY_STALE,
                    format!(
                        "pending change '{id}' was proposed against entry {entry}, which has changed since; nothing was applied"
                    ),
                )
                .with_hint(format!("prometheus memory reject {id}")));
            }
        }
        let ctx = context(config, target, pending.origin, pending.session_id.clone());
        let mut entries = b.entries.clone();
        let applied = batch::apply(&mut entries, &operations(&pending)?, &ctx).map_err(refused)?;
        b.entries = entries;
        b.resolve.push(id.to_owned());
        Ok(Written {
            results: applied.results,
            staged: Vec::new(),
            usage: applied.usage,
            pending_id: None,
        })
    })
}

/// Drops staged change `id`.
///
/// # Errors
///
/// `memory.pending_not_found`; store errors.
pub fn reject(store: &Store, id: &str) -> Result<MemoryPending, Error> {
    store.memory_batch(None, |b| {
        let pending = b
            .pending
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| not_found(id))?;
        b.resolve.push(id.to_owned());
        Ok(pending)
    })
}

/// One line per operation of `pending`: `add: <content>`, `remove m_x`.
#[must_use]
pub fn describe(pending: &MemoryPending) -> Vec<String> {
    let ops: Vec<Op> = serde_json::from_value(pending.operations.clone()).unwrap_or_default();
    let base: BTreeMap<String, String> =
        serde_json::from_value(pending.base.clone()).unwrap_or_default();
    ops.iter()
        .map(|op| {
            let id = op.id.as_deref().unwrap_or_default();
            let was = base
                .get(id)
                .map(|c| format!(" (was: {c})"))
                .unwrap_or_default();
            match &op.content {
                Some(content) if id.is_empty() => format!("{}: {content}", op.action.as_str()),
                Some(content) => format!("{} {id}: {content}{was}", op.action.as_str()),
                None => format!("{} {id}{was}", op.action.as_str()),
            }
        })
        .collect()
}

/// The JSON form of a staged change.
#[must_use]
pub fn pending_json(pending: &MemoryPending) -> Value {
    serde_json::json!({
        "id": pending.id,
        "target": pending.target,
        "origin": pending.origin,
        "session_id": pending.session_id,
        "created_at": pending.created_at.to_string(),
        "operations": pending.operations,
        "base": pending.base,
    })
}
