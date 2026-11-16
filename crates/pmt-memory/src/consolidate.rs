//! Consolidation proposals (spec: memory, Consolidation proposals).
//!
//! The auxiliary memory model reads a target's entries and answers JSON
//! proposals: merges of duplicates, resolutions of contradictions and
//! promotions or demotions between tiers. [`plan`] turns them into memory
//! operations under rules the model cannot bend: a pinned entry is never
//! removed, a contradiction keeps the newer entry unless the older one is
//! pinned, and unknown ids are skipped. The plan is shown as a diff of the
//! Markdown view ([`preview`]); [`commit`] applies it with
//! `origin = consolidation` (journaled), [`stage`] queues it for
//! `memory approve`.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use jiff::Timestamp;
use pmt_config::schema::MemoryConfig;
use pmt_core::error::{Error, codes};
use pmt_store::{MemoryEntry, MemoryOrigin, MemoryStatus, MemoryTarget, MemoryTier, Store};
use serde::{Deserialize, Serialize};

use crate::batch;
use crate::ops::{Action, Op};
use crate::write::{self, Written};
use crate::{budget, markdown};

/// The system prompt of a consolidation call.
pub const INSTRUCTIONS: &str = "You consolidate an assistant's long-term memory. You get the \
entries of one store with their ids, tier (core entries are always in the prompt and share a \
character budget; archival entries are recalled when relevant), kind, pinned flag and dates. \
Propose only clear improvements: merge entries that say the same thing, resolve entries that \
contradict each other, promote archival entries that belong in every conversation and demote \
core entries that do not when the core tier is over its budget. Answer with one JSON object \
and nothing else: {\"changes\": [{\"action\": \"merge\", \"ids\": [\"m_...\", \"m_...\"], \
\"content\": \"merged text\"}, {\"action\": \"resolve\", \"ids\": [\"m_...\", \"m_...\"]}, \
{\"action\": \"promote\", \"id\": \"m_...\"}, {\"action\": \"demote\", \"id\": \"m_...\"}]}. \
Every change may carry a short \"reason\". Answer {\"changes\": []} when nothing should change.";

/// What one proposal does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Several entries become one with `content`.
    Merge,
    /// Contradicting entries: one is kept, the others removed.
    Resolve,
    /// An archival entry moves to the core tier.
    Promote,
    /// A core entry moves to the archival tier.
    Demote,
}

/// One change the model proposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    /// What it does.
    pub action: Kind,
    /// The entries a merge or resolution covers.
    #[serde(default)]
    pub ids: Vec<String>,
    /// The entry a promotion or demotion moves.
    #[serde(default)]
    pub id: Option<String>,
    /// The merged text.
    #[serde(default)]
    pub content: Option<String>,
    /// Why.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The user message of a consolidation call over `entries` of `target`.
#[must_use]
pub fn prompt(entries: &[MemoryEntry], config: &MemoryConfig, target: MemoryTarget) -> String {
    let mut text = format!(
        "Store: {target}. Core budget: {} of {} characters used.\n\nEntries:\n",
        budget::core_chars(entries, target),
        budget::limit(config, target)
    );
    for e in entries.iter().filter(|e| eligible(e, target)) {
        let _ = writeln!(
            text,
            "- id={} tier={} kind={} pinned={} created={} updated={}\n  {}",
            e.id,
            e.tier,
            e.kind,
            e.pinned,
            e.created_at.strftime("%Y-%m-%d"),
            e.updated_at.strftime("%Y-%m-%d"),
            e.content.replace('\n', "\n  ")
        );
    }
    text
}

fn eligible(entry: &MemoryEntry, target: MemoryTarget) -> bool {
    entry.target == target && entry.status == MemoryStatus::Active
}

/// The proposals in a model reply (the first JSON object in it).
///
/// # Errors
///
/// `memory.invalid` for a reply that holds no proposal list.
pub fn parse(reply: &str) -> Result<Vec<Proposal>, Error> {
    #[derive(Deserialize)]
    struct Reply {
        changes: Vec<Proposal>,
    }
    let json = match (reply.find('{'), reply.rfind('}')) {
        (Some(s), Some(e)) if e > s => &reply[s..=e],
        _ => reply,
    };
    serde_json::from_str::<Reply>(json)
        .map(|r| r.changes)
        .map_err(|e| {
            Error::new(
                &codes::MEMORY_INVALID,
                format!("the consolidation model answered no change list: {e}"),
            )
        })
}

/// The operations of one target and the proposals that were set aside.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// The target.
    pub target: Option<MemoryTarget>,
    /// The operations, in order.
    pub ops: Vec<Op>,
    /// Why each skipped proposal was skipped.
    pub skipped: Vec<String>,
}

impl Plan {
    /// Whether nothing would change.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

fn on(action: Action, id: &str) -> Op {
    Op {
        id: Some(id.to_owned()),
        ..Op::new(action)
    }
}

/// The newest of `group` wins, unless an older one is pinned: then the
/// (newest) pinned one does.
fn winner<'a>(group: &[&'a MemoryEntry]) -> &'a MemoryEntry {
    let newest = |a: &&&MemoryEntry, b: &&&MemoryEntry| {
        a.updated_at
            .cmp(&b.updated_at)
            .then(a.created_at.cmp(&b.created_at))
    };
    group
        .iter()
        .filter(|e| e.pinned)
        .max_by(newest)
        .or_else(|| group.iter().max_by(newest))
        .copied()
        .unwrap_or(group[0])
}

/// The entries `ids` names, or why the proposal is skipped.
fn group<'a>(
    entries: &'a [MemoryEntry],
    target: MemoryTarget,
    ids: &[String],
    used: &BTreeSet<String>,
) -> Result<Vec<&'a MemoryEntry>, String> {
    let unique: BTreeSet<&String> = ids.iter().collect();
    if unique.len() < 2 {
        return Err("names fewer than two entries".to_owned());
    }
    unique
        .into_iter()
        .map(|id| {
            if used.contains(id) {
                return Err(format!("{id} is already changed by another proposal"));
            }
            entries
                .iter()
                .find(|e| e.id == *id && eligible(e, target))
                .ok_or_else(|| format!("no active {target} entry {id}"))
        })
        .collect()
}

/// Turns `proposals` into operations on `target`'s `entries`.
#[must_use]
pub fn plan(entries: &[MemoryEntry], target: MemoryTarget, proposals: &[Proposal]) -> Plan {
    let mut plan = Plan {
        target: Some(target),
        ..Plan::default()
    };
    let mut used = BTreeSet::new();
    for (n, proposal) in proposals.iter().enumerate() {
        match one(entries, target, proposal, &used) {
            Ok(ops) => {
                used.extend(ops.iter().filter_map(|op| op.id.clone()));
                plan.ops.extend(ops);
            }
            Err(why) => {
                plan.skipped
                    .push(format!("change {} ({:?}): {why}", n + 1, proposal.action));
            }
        }
    }
    plan
}

fn one(
    entries: &[MemoryEntry],
    target: MemoryTarget,
    proposal: &Proposal,
    used: &BTreeSet<String>,
) -> Result<Vec<Op>, String> {
    match proposal.action {
        Kind::Merge | Kind::Resolve => {
            let group = group(entries, target, &proposal.ids, used)?;
            merge_ops(&group, proposal)
        }
        Kind::Promote | Kind::Demote => tier_op(entries, target, proposal, used).map(|op| vec![op]),
    }
}

/// A merge or resolution of `group`: the winner stays (a merge patches it
/// with the merged text), the others go.
fn merge_ops(group: &[&MemoryEntry], proposal: &Proposal) -> Result<Vec<Op>, String> {
    if group.iter().filter(|e| e.pinned).count() > 1 {
        return Err("more than one of its entries is pinned".to_owned());
    }
    let keep = winner(group);
    let mut ops = Vec::new();
    if proposal.action == Kind::Merge {
        let content = proposal
            .content
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or("a merge needs the merged content")?;
        // A pinned survivor keeps its own words.
        if content != keep.content && !keep.pinned {
            ops.push(Op {
                content: Some(content.to_owned()),
                ..on(Action::Patch, &keep.id)
            });
        }
    }
    ops.extend(
        group
            .iter()
            .filter(|e| e.id != keep.id)
            .map(|e| on(Action::Remove, &e.id)),
    );
    Ok(ops)
}

/// A promotion or demotion between tiers.
fn tier_op(
    entries: &[MemoryEntry],
    target: MemoryTarget,
    proposal: &Proposal,
    used: &BTreeSet<String>,
) -> Result<Op, String> {
    let id = proposal.id.as_deref().ok_or("names no entry")?;
    if used.contains(id) {
        return Err(format!("{id} is already changed by another proposal"));
    }
    let entry = entries
        .iter()
        .find(|e| e.id == id && eligible(e, target))
        .ok_or_else(|| format!("no active {target} entry {id}"))?;
    let tier = if proposal.action == Kind::Promote {
        MemoryTier::Core
    } else {
        MemoryTier::Archival
    };
    if entry.tier == tier {
        return Err(format!("{id} is already {tier}"));
    }
    Ok(Op {
        tier: Some(tier),
        ..on(Action::Patch, id)
    })
}

fn context(config: &MemoryConfig, target: MemoryTarget, now: Timestamp) -> batch::Context {
    batch::Context {
        now,
        ..write::context(config, target, MemoryOrigin::Consolidation, None)
    }
}

/// The unified diff `plan` would make to the Markdown view of its target.
///
/// # Errors
///
/// `memory.invalid` / `memory.over_budget` for a plan that cannot apply
/// (a final core tier over budget); store errors.
pub fn preview(
    store: &Store,
    config: &MemoryConfig,
    plan: &Plan,
    now: Timestamp,
) -> Result<String, Error> {
    let Some(target) = plan.target else {
        return Ok(String::new());
    };
    let before = store.memory_entries(Some(target))?;
    let mut after = before.clone();
    batch::apply(&mut after, &plan.ops, &context(config, target, now))
        .map_err(|m| Error::new(&codes::MEMORY_INVALID, m))?;
    let (old, new) = (
        markdown::render(&before, &[target]),
        markdown::render(&after, &[target]),
    );
    Ok(if old == new {
        String::new()
    } else {
        markdown::diff(&old, &new)
    })
}

/// Commits `plan` as one batch journaled with `origin = consolidation`.
///
/// # Errors
///
/// As [`write::write`]; nothing is written on error.
pub fn commit(
    store: &Store,
    config: &MemoryConfig,
    plan: &Plan,
    now: Timestamp,
) -> Result<Option<Written>, Error> {
    match plan.target {
        Some(target) if !plan.is_empty() => Ok(Some(write::write(
            store,
            &context(config, target, now),
            &plan.ops,
            &[],
        )?)),
        _ => Ok(None),
    }
}

/// Stages `plan` for `memory approve` (origin `consolidation`); the
/// pending id.
///
/// # Errors
///
/// As [`write::write`].
pub fn stage(
    store: &Store,
    config: &MemoryConfig,
    plan: &Plan,
    now: Timestamp,
) -> Result<Option<String>, Error> {
    match plan.target {
        Some(target) if !plan.is_empty() => {
            Ok(write::write(store, &context(config, target, now), &[], &plan.ops)?.pending_id)
        }
        _ => Ok(None),
    }
}

/// The `store_meta` preference holding the last idle-time pass.
pub const LAST_RUN: &str = "memory.consolidation.last_run";

/// How long the profile must have been quiet for an idle-time pass.
pub const MIN_IDLE_SECS: i64 = 900;

/// Whether an idle-time pass is due at `now`: enabled, the last one at
/// least `interval_secs` ago, and no session active in the last
/// [`MIN_IDLE_SECS`] (`last_activity`). The first observation only seeds
/// the time, so a new profile is not consolidated the moment it appears.
///
/// # Errors
///
/// Store errors.
pub fn due(
    store: &Store,
    config: &MemoryConfig,
    now: Timestamp,
    last_activity: Option<Timestamp>,
) -> Result<bool, Error> {
    if !config.consolidation.enabled {
        return Ok(false);
    }
    let last = store
        .pref(LAST_RUN)?
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|ms| Timestamp::from_millisecond(ms).ok());
    let Some(last) = last else {
        mark_run(store, now)?;
        return Ok(false);
    };
    let interval = i64::try_from(config.consolidation.interval_secs).unwrap_or(i64::MAX);
    let idle = last_activity.is_none_or(|at| now.duration_since(at).as_secs() >= MIN_IDLE_SECS);
    Ok(idle && now.duration_since(last).as_secs() >= interval)
}

/// Records an idle-time pass at `now`.
///
/// # Errors
///
/// Store errors.
pub fn mark_run(store: &Store, now: Timestamp) -> Result<(), Error> {
    store.set_pref(LAST_RUN, &now.as_millisecond().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmt_store::MemoryKind;

    fn entry(id: &str, content: &str, updated: i64, pinned: bool) -> MemoryEntry {
        let at = Timestamp::from_second(updated).unwrap();
        MemoryEntry {
            id: id.into(),
            target: MemoryTarget::User,
            kind: MemoryKind::Preference,
            tier: MemoryTier::Core,
            content: content.into(),
            pinned,
            status: MemoryStatus::Active,
            expires_at: None,
            created_at: at,
            updated_at: at,
            last_recalled_at: None,
            recall_count: 0,
            origin: MemoryOrigin::User,
            origin_session: None,
        }
    }

    fn resolve(a: &str, b: &str) -> Proposal {
        Proposal {
            action: Kind::Resolve,
            ids: vec![a.into(), b.into()],
            id: None,
            content: None,
            reason: None,
        }
    }

    #[test]
    fn a_contradiction_keeps_the_newer_entry_unless_the_older_is_pinned() {
        let old = entry("m_aaaaaaaaaaaa", "Uses vim.", 1_740_000_000, false);
        let new = entry("m_bbbbbbbbbbbb", "Switched to Helix.", 1_770_000_000, false);
        // Whichever order the model names them in, the older one goes.
        let p = plan(
            &[old.clone(), new.clone()],
            MemoryTarget::User,
            &[resolve(&new.id, &old.id)],
        );
        assert_eq!(p.ops, [on(Action::Remove, &old.id)]);
        let pinned_old = MemoryEntry {
            pinned: true,
            ..old.clone()
        };
        let p = plan(
            &[pinned_old, new.clone()],
            MemoryTarget::User,
            &[resolve(&old.id, &new.id)],
        );
        assert_eq!(p.ops, [on(Action::Remove, &new.id)]);
    }

    #[test]
    fn unknown_ids_and_reused_entries_are_skipped() {
        let a = entry("m_aaaaaaaaaaaa", "A", 1_740_000_000, false);
        let b = entry("m_bbbbbbbbbbbb", "B", 1_750_000_000, false);
        let p = plan(
            &[a.clone(), b.clone()],
            MemoryTarget::User,
            &[
                resolve(&a.id, "m_zzzzzzzzzzzz"),
                resolve(&a.id, &b.id),
                resolve(&a.id, &b.id),
            ],
        );
        assert_eq!(p.ops.len(), 1);
        assert_eq!(p.skipped.len(), 2, "{:?}", p.skipped);
    }

    #[test]
    fn a_reply_parses_from_surrounding_text() {
        let got = parse("Here: {\"changes\": [{\"action\": \"promote\", \"id\": \"m_x\"}]} done")
            .unwrap();
        assert_eq!(got[0].action, Kind::Promote);
        assert!(parse("nothing").is_err());
    }
}
