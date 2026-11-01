//! Applying a batch of operations to one target's entries in memory
//! (spec: memory, Atomic batches judged on the final state; Entry matching
//! for patch and remove; Exact duplicate add is a no-op).
//!
//! [`apply`] works on a copy: any failing operation, or a final state over
//! a budget, fails the whole batch and the caller writes nothing.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use jiff::Timestamp;
use pmt_store::{MemoryEntry, MemoryKind, MemoryOrigin, MemoryStatus, MemoryTarget, MemoryTier};
use serde::Serialize;

use crate::budget::{self, Usage};
use crate::ops::{Action, Op};

/// Who writes and under which limits.
#[derive(Debug, Clone)]
pub struct Context {
    /// The target every operation applies to.
    pub target: MemoryTarget,
    /// Recorded on new entries.
    pub origin: MemoryOrigin,
    /// The writing session, when one exists.
    pub session: Option<String>,
    /// The time recorded on changes.
    pub now: Timestamp,
    /// The target's core character budget.
    pub core_limit: usize,
    /// Most archival entries of the target.
    pub archival_max: usize,
}

/// What one operation did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpResult {
    /// The action.
    pub action: Action,
    /// The affected (or, for a duplicate add, the existing) entry.
    pub id: String,
    /// An add that matched an active entry exactly.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub duplicate: bool,
}

/// A batch that fits.
#[derive(Debug, Clone)]
pub struct Applied {
    /// One result per operation, in order.
    pub results: Vec<OpResult>,
    /// The operations with every selector resolved to an id (duplicate
    /// adds dropped): the form a staged batch is approved in.
    pub resolved: Vec<Op>,
    /// The content each touched pre-existing entry had before the batch.
    pub base: BTreeMap<String, String>,
    /// The target's usage after the batch.
    pub usage: Usage,
}

/// Applies `ops` to `entries` (all of `ctx.target`'s entries).
///
/// # Errors
///
/// The refusal shown to the model; `entries` is then unusable and must be
/// discarded.
pub fn apply(entries: &mut Vec<MemoryEntry>, ops: &[Op], ctx: &Context) -> Result<Applied, String> {
    let before = budget::core_chars(entries, ctx.target);
    let mut applied = apply_unchecked(entries, ops, ctx)?;
    applied.usage = check_limits(entries, ctx, before)?;
    Ok(applied)
}

/// [`apply`] without the final budget and archival checks (callers that
/// add entries of their own first run [`check_limits`] themselves).
///
/// # Errors
///
/// The refusal of an operation that cannot apply.
pub fn apply_unchecked(
    entries: &mut Vec<MemoryEntry>,
    ops: &[Op],
    ctx: &Context,
) -> Result<Applied, String> {
    let original: BTreeMap<String, String> = entries
        .iter()
        .map(|e| (e.id.clone(), e.content.clone()))
        .collect();
    let mut applied = Applied {
        results: Vec::with_capacity(ops.len()),
        resolved: Vec::new(),
        base: BTreeMap::new(),
        usage: Usage::new(0, ctx.core_limit),
    };
    for (i, op) in ops.iter().enumerate() {
        let (result, resolved) = match op.action {
            Action::Add => add(entries, op, ctx),
            _ => change(entries, i, op, ctx)?,
        };
        if let Some(resolved) = resolved {
            if let Some(id) = resolved.id.as_ref().filter(|_| op.action.selects())
                && let Some(content) = original.get(id)
            {
                applied
                    .base
                    .entry(id.clone())
                    .or_insert_with(|| content.clone());
            }
            applied.resolved.push(resolved);
        }
        applied.results.push(result);
    }
    Ok(applied)
}

/// The target's final usage, or the refusal of a final state whose core
/// tier grew past its budget (from `before` characters) or whose archival
/// tier holds too many entries.
///
/// # Errors
///
/// The refusal text, naming current and limit characters.
pub fn check_limits(
    entries: &[MemoryEntry],
    ctx: &Context,
    before: usize,
) -> Result<Usage, String> {
    let after = budget::core_chars(entries, ctx.target);
    if after > ctx.core_limit && after > before {
        return Err(over_budget(ctx, before, after));
    }
    let archival = entries
        .iter()
        .filter(|e| e.target == ctx.target && e.tier == MemoryTier::Archival)
        .count();
    if archival > ctx.archival_max {
        return Err(format!(
            "the {} archival tier would hold {archival} entries (limit {}, memory.archival_max_entries); remove stale archival entries in the same call",
            ctx.target, ctx.archival_max
        ));
    }
    Ok(Usage::new(after, ctx.core_limit))
}

fn over_budget(ctx: &Context, before: usize, after: usize) -> String {
    let limit = ctx.core_limit;
    format!(
        "the {target} core tier is at {before}/{limit} chars and this call would make it {after}/{limit}. \
         Nothing was written. Consolidate in one call (one `operations` list that removes or patches stale \
         entries and then adds), or store the entry with `tier: \"archival\"` (recalled per turn, no budget).",
        target = ctx.target
    )
}

fn add(entries: &mut Vec<MemoryEntry>, op: &Op, ctx: &Context) -> (OpResult, Option<Op>) {
    let content = op.content.clone().unwrap_or_default();
    if let Some(existing) = entries.iter().find(|e| {
        e.target == ctx.target && e.status == MemoryStatus::Active && e.content.trim() == content
    }) {
        let result = OpResult {
            action: Action::Add,
            id: existing.id.clone(),
            duplicate: true,
        };
        return (result, None);
    }
    let entry = MemoryEntry {
        id: crate::id::entry(),
        target: ctx.target,
        kind: op.kind.unwrap_or(MemoryKind::Fact),
        tier: op.tier.unwrap_or(MemoryTier::Core),
        content,
        pinned: false,
        status: MemoryStatus::Active,
        expires_at: op.expires_at,
        created_at: ctx.now,
        updated_at: ctx.now,
        last_recalled_at: None,
        recall_count: 0,
        origin: ctx.origin,
        origin_session: ctx.session.clone(),
    };
    let result = OpResult {
        action: Action::Add,
        id: entry.id.clone(),
        duplicate: false,
    };
    entries.push(entry);
    (result, Some(op.clone()))
}

fn change(
    entries: &mut Vec<MemoryEntry>,
    i: usize,
    op: &Op,
    ctx: &Context,
) -> Result<(OpResult, Option<Op>), String> {
    let at = select(entries, i, op, ctx.target)?;
    let id = entries[at].id.clone();
    match op.action {
        Action::Remove => {
            entries.remove(at);
        }
        Action::Pin | Action::Unpin => {
            entries[at].pinned = op.action == Action::Pin;
            entries[at].updated_at = ctx.now;
        }
        Action::Patch | Action::Add => {
            let entry = &mut entries[at];
            if let Some(content) = &op.content {
                entry.content.clone_from(content);
            }
            entry.kind = op.kind.unwrap_or(entry.kind);
            entry.tier = op.tier.unwrap_or(entry.tier);
            entry.expires_at = op.expires_at.or(entry.expires_at);
            entry.updated_at = ctx.now;
        }
    }
    let mut resolved = op.clone();
    resolved.id = Some(id.clone());
    resolved.old_text = None;
    let result = OpResult {
        action: op.action,
        id,
        duplicate: false,
    };
    Ok((result, Some(resolved)))
}

/// The index of the entry `op` names: by `id` (any entry of the target),
/// else by `old_text` matching exactly one active entry.
fn select(
    entries: &[MemoryEntry],
    i: usize,
    op: &Op,
    target: MemoryTarget,
) -> Result<usize, String> {
    let at = format!("operations[{i}]");
    if let Some(id) = &op.id {
        return entries
            .iter()
            .position(|e| e.target == target && e.id == *id)
            .ok_or_else(|| {
                format!(
                    "{at}: no {target} entry has id '{id}'.{}",
                    listing(entries, target)
                )
            });
    }
    let Some(needle) = &op.old_text else {
        return Err(format!(
            "{at}: {} needs the entry's `id` (or an `old_text` that matches exactly one entry).{} Retry with one of these ids.",
            op.action.as_str(),
            listing(entries, target)
        ));
    };
    let matches: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e.target == target
                && e.status == MemoryStatus::Active
                && e.content.contains(needle.as_str())
        })
        .map(|(n, _)| n)
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!(
            "{at}: no active {target} entry contains '{needle}'.{}",
            listing(entries, target)
        )),
        several => Err(format!(
            "{at}: '{needle}' matches {} entries: {}. Nothing was written; retry with `id`.",
            several.len(),
            several
                .iter()
                .map(|&n| entries[n].id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The target's active entries with their ids, for a retry.
fn listing(entries: &[MemoryEntry], target: MemoryTarget) -> String {
    let mut text = String::from(" Current entries:");
    let mut any = false;
    for e in entries
        .iter()
        .filter(|e| e.target == target && e.status == MemoryStatus::Active)
    {
        any = true;
        let preview: String = e.content.chars().take(80).collect();
        let _ = write!(text, "\n- {}: {preview}", e.id);
    }
    if !any {
        text.push_str(" (none)");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(limit: usize) -> Context {
        Context {
            target: MemoryTarget::User,
            origin: MemoryOrigin::Agent,
            session: None,
            now: Timestamp::now(),
            core_limit: limit,
            archival_max: 2,
        }
    }

    fn op(action: Action) -> Op {
        Op::new(action)
    }

    fn add_op(content: &str) -> Op {
        Op {
            content: Some(content.into()),
            ..op(Action::Add)
        }
    }

    fn seeded(contents: &[&str], limit: usize) -> Vec<MemoryEntry> {
        let mut entries = Vec::new();
        let ops: Vec<Op> = contents.iter().map(|c| add_op(c)).collect();
        apply(&mut entries, &ops, &ctx(limit)).unwrap();
        entries
    }

    #[test]
    fn over_budget_add_names_usage_and_archival() {
        // 2150 chars: one entry of 2150.
        let mut entries = seeded(&[&"x".repeat(2150)], 2200);
        let message = apply(&mut entries, &[add_op(&"y".repeat(120))], &ctx(2200)).unwrap_err();
        assert!(message.contains("2150/2200"), "{message}");
        assert!(message.contains("tier: \"archival\""), "{message}");
    }

    #[test]
    fn remove_then_add_is_judged_on_the_final_state() {
        let stale = "s".repeat(300);
        let mut entries = seeded(&[&"k".repeat(1897), &stale], 2200);
        assert_eq!(
            budget::core_chars(&entries, MemoryTarget::User),
            2200,
            "full"
        );
        assert!(
            apply(
                &mut entries.clone(),
                &[add_op(&"n".repeat(250))],
                &ctx(2200)
            )
            .is_err()
        );
        let remove = Op {
            old_text: Some(stale.clone()),
            ..op(Action::Remove)
        };
        let applied = apply(
            &mut entries,
            &[remove, add_op(&"n".repeat(250))],
            &ctx(2200),
        )
        .unwrap();
        assert_eq!(applied.results.len(), 2);
        assert!(entries.iter().all(|e| e.content != stale));
        assert_eq!(applied.base.values().next(), Some(&stale));
    }

    #[test]
    fn ambiguous_old_text_lists_both_ids() {
        let mut entries = seeded(
            &["Uses Python 3.12.", "Python scripts live in tools/."],
            2200,
        );
        let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
        let remove = Op {
            old_text: Some("Python".into()),
            ..op(Action::Remove)
        };
        let message = apply(&mut entries.clone(), &[remove], &ctx(2200)).unwrap_err();
        assert!(
            message.contains(&ids[0]) && message.contains(&ids[1]),
            "{message}"
        );
        assert_eq!(entries.len(), 2);
        let missing = apply(&mut entries, &[op(Action::Remove)], &ctx(2200)).unwrap_err();
        assert!(
            missing.contains(&ids[0]) && missing.contains("Retry"),
            "{missing}"
        );
    }

    #[test]
    fn duplicate_add_returns_the_existing_id() {
        let mut entries = seeded(&["Prefers metric units."], 2200);
        let id = entries[0].id.clone();
        let applied = apply(
            &mut entries,
            &[add_op("Prefers metric units."), add_op("Tea.")],
            &ctx(2200),
        )
        .unwrap();
        assert_eq!(
            applied.results[0],
            OpResult {
                action: Action::Add,
                id,
                duplicate: true
            }
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(applied.resolved.len(), 1);
    }

    #[test]
    fn archival_entries_skip_the_budget_but_not_the_cap() {
        let archival = |c: &str| Op {
            tier: Some(MemoryTier::Archival),
            ..add_op(c)
        };
        let mut entries = Vec::new();
        apply(
            &mut entries,
            &[archival(&"a".repeat(5000)), archival("b")],
            &ctx(10),
        )
        .unwrap();
        let message = apply(&mut entries, &[archival("c")], &ctx(10)).unwrap_err();
        assert!(message.contains("archival_max_entries"), "{message}");
    }
}
