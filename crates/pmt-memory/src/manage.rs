//! Changes the user makes outside a turn: applying an edited Markdown view
//! and importing files (spec: memory, Memory CLI and editing).
//!
//! Both run as one store transaction over every target they touch and are
//! judged on the final state like a tool batch. An edit is the user's own
//! writing (`origin = user`) and is threat-scanned like any write; an
//! import (`origin = import`) keeps an entry that matches a threat pattern
//! as `blocked`, so the user can inspect it with `memory show` and remove
//! it, and never stores one that holds a credential.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use pmt_config::schema::MemoryConfig;
use pmt_core::error::{Error, codes};
use pmt_core::secret::Redactor;
use pmt_store::{
    MemoryEntry, MemoryKind, MemoryOrigin, MemoryStatus, MemoryTarget, MemoryTier, Store,
};
use serde::{Deserialize, Serialize};

use crate::batch::{self, Context};
use crate::markdown::Section;
use crate::ops::{Action, Op};
use crate::{budget, scan, write};

fn invalid(message: impl Into<String>) -> Error {
    Error::new(&codes::MEMORY_INVALID, message)
}

fn refused(message: String) -> Error {
    if message.contains("core tier is at") || message.contains("archival_max_entries") {
        Error::new(&codes::MEMORY_OVER_BUDGET, message)
    } else {
        invalid(message)
    }
}

/// What a user change did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Entries created.
    pub added: usize,
    /// Entries changed.
    pub updated: usize,
    /// Entries removed.
    pub removed: usize,
    /// Items skipped as exact duplicates of active entries.
    pub duplicates: usize,
    /// Imported entries kept as `blocked` by the threat scan.
    pub blocked: Vec<String>,
    /// Items skipped because they hold a credential.
    pub secrets_skipped: usize,
}

impl Summary {
    /// Whether nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added + self.updated + self.removed == 0 && self.blocked.is_empty()
    }
}

/// The operations that turn `current` (the entries of the edited targets)
/// into `sections`.
fn edit_ops(
    current: &[MemoryEntry],
    targets: &[MemoryTarget],
    sections: &[Section],
) -> Result<BTreeMap<MemoryTarget, Vec<Op>>, Error> {
    let mut ops: BTreeMap<MemoryTarget, Vec<Op>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for s in sections {
        if !targets.contains(&s.target) {
            return Err(invalid(format!(
                "the view edits only {}; remove the {} section",
                list(targets),
                s.target
            )));
        }
        if let Some(id) = &s.id
            && !seen.insert(id.clone())
        {
            return Err(invalid(format!("entry {id} appears twice")));
        }
        ops.entry(s.target)
            .or_default()
            .extend(section_ops(current, targets, s)?);
    }
    for entry in current
        .iter()
        .filter(|e| targets.contains(&e.target) && !seen.contains(&e.id))
    {
        ops.entry(entry.target)
            .or_default()
            .push(on(Action::Remove, &entry.id));
    }
    ops.retain(|_, v| !v.is_empty());
    Ok(ops)
}

fn on(action: Action, id: &str) -> Op {
    let mut op = Op::new(action);
    op.id = Some(id.to_owned());
    op
}

/// An operation writing `content`, `kind` and `tier` (an add without `id`).
fn writing(
    action: Action,
    id: Option<&str>,
    content: &str,
    kind: MemoryKind,
    tier: MemoryTier,
) -> Op {
    let mut op = Op::new(action);
    op.id = id.map(str::to_owned);
    op.content = Some(content.to_owned());
    op.kind = Some(kind);
    op.tier = Some(tier);
    op
}

/// The operations one edited section asks for.
fn section_ops(
    current: &[MemoryEntry],
    targets: &[MemoryTarget],
    s: &Section,
) -> Result<Vec<Op>, Error> {
    let Some(id) = &s.id else {
        return Ok(vec![writing(Action::Add, None, &s.content, s.kind, s.tier)]);
    };
    let entry = current
        .iter()
        .find(|e| e.id == *id)
        .ok_or_else(|| invalid(format!("section {id} names no entry of {}", list(targets))))?;
    if entry.target != s.target {
        return Err(invalid(format!(
            "entry {id} belongs to {}: to move it, delete its section and add a `## new` one",
            entry.target
        )));
    }
    let mut ops = Vec::new();
    if entry.content != s.content || entry.kind != s.kind || entry.tier != s.tier {
        ops.push(writing(Action::Patch, Some(id), &s.content, s.kind, s.tier));
    }
    if entry.pinned != s.pinned {
        ops.push(on(if s.pinned { Action::Pin } else { Action::Unpin }, id));
    }
    Ok(ops)
}

fn list(targets: &[MemoryTarget]) -> String {
    targets
        .iter()
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(" and ")
}

fn count(summary: &mut Summary, action: Action, duplicate: bool) {
    match (action, duplicate) {
        (Action::Add, true) => summary.duplicates += 1,
        (Action::Add, false) => summary.added += 1,
        (Action::Remove, _) => summary.removed += 1,
        _ => summary.updated += 1,
    }
}

/// Applies an edited view of `targets` as one batch (`origin = user`).
///
/// # Errors
///
/// `memory.invalid` (a section naming no entry, a refused write),
/// `memory.blocked` (threat or secret scan), `memory.over_budget`; store
/// errors. Nothing is written on error.
pub fn apply_edit(
    store: &Store,
    config: &MemoryConfig,
    targets: &[MemoryTarget],
    sections: &[Section],
    redactor: &Redactor,
) -> Result<Summary, Error> {
    for s in sections {
        scan::check(&s.content, redactor).map_err(|m| {
            Error::new(
                &codes::MEMORY_BLOCKED,
                format!("entry {}: {m}", s.id.as_deref().unwrap_or("new")),
            )
        })?;
    }
    store.memory_batch(None, |b| {
        let plan = edit_ops(&b.entries, targets, sections)?;
        let mut entries = b.entries.clone();
        let mut summary = Summary::default();
        for (target, ops) in &plan {
            let ctx = write::context(config, *target, MemoryOrigin::User, None);
            let applied = batch::apply(&mut entries, ops, &ctx).map_err(refused)?;
            for r in &applied.results {
                count(&mut summary, r.action, r.duplicate);
            }
        }
        b.entries = entries;
        Ok(summary)
    })
}

/// One item read from an import file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// The entry's id when the file carries one (a Prometheus export).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The target.
    pub target: MemoryTarget,
    /// The kind.
    #[serde(default = "default_kind")]
    pub kind: MemoryKind,
    /// The tier.
    #[serde(default = "default_tier")]
    pub tier: MemoryTier,
    /// Pinned.
    #[serde(default)]
    pub pinned: bool,
    /// The text.
    pub content: String,
}

fn default_kind() -> MemoryKind {
    MemoryKind::Fact
}

fn default_tier() -> MemoryTier {
    MemoryTier::Core
}

impl From<Section> for Item {
    fn from(s: Section) -> Self {
        Self {
            id: s.id,
            target: s.target,
            kind: s.kind,
            tier: s.tier,
            pinned: s.pinned,
            content: s.content,
        }
    }
}

/// Reads an import file: a Prometheus JSONL or Markdown export, or a
/// Hermes `MEMORY.md`/`USER.md` (entries separated by `§` lines), whose
/// target is `target` or inferred from the file name. Hermes entries get
/// `tier`.
///
/// # Errors
///
/// `memory.invalid` for a file that does not read.
pub fn read(
    path: &Path,
    text: &str,
    target: Option<MemoryTarget>,
    tier: MemoryTier,
) -> Result<Vec<Item>, Error> {
    let text = text.replace("\r\n", "\n");
    let shown = path.display();
    if path.extension().is_some_and(|e| e == "jsonl") {
        return text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(n, l)| {
                serde_json::from_str(l).map_err(|e| invalid(format!("{shown}:{}: {e}", n + 1)))
            })
            .collect();
    }
    if text
        .lines()
        .any(|l| l.starts_with("## m_") || l.trim() == "## new")
    {
        let sections =
            crate::markdown::parse(&text).map_err(|m| invalid(format!("{shown}: {m}")))?;
        return Ok(sections
            .into_iter()
            .filter(|s| target.is_none_or(|t| s.target == t))
            .map(Item::from)
            .collect());
    }
    let target = target.or_else(|| infer_target(path)).ok_or_else(|| {
        invalid(format!(
            "{shown}: pass --target memory|user (the file name does not say which store it is)"
        ))
    })?;
    Ok(text
        .split("\n§\n")
        .map(|chunk| chunk.trim().trim_matches('§').trim())
        .filter(|chunk| !chunk.is_empty())
        .map(|content| Item {
            id: None,
            target,
            kind: MemoryKind::Fact,
            tier,
            pinned: false,
            content: content.to_owned(),
        })
        .collect())
}

fn infer_target(path: &Path) -> Option<MemoryTarget> {
    let stem = path.file_stem()?.to_str()?.to_ascii_lowercase();
    match stem.as_str() {
        "memory" => Some(MemoryTarget::Memory),
        "user" => Some(MemoryTarget::User),
        _ => None,
    }
}

/// Imports `items` as one batch: exact duplicates of active entries are
/// skipped, an id the store has is updated in place, an id it lacks is
/// restored as is, credentials are skipped and threat matches are kept
/// `blocked`.
///
/// # Errors
///
/// `memory.over_budget`, `memory.invalid`; store errors. Nothing is
/// written on error.
pub fn import(
    store: &Store,
    config: &MemoryConfig,
    items: &[Item],
    redactor: &Redactor,
) -> Result<Summary, Error> {
    let mut summary = Summary::default();
    let kept: Vec<&Item> = items
        .iter()
        .filter(|item| {
            let secret = scan::has_secret(&item.content, redactor);
            summary.secrets_skipped += usize::from(secret);
            !secret
        })
        .collect();
    let skipped = summary.secrets_skipped;
    store.memory_batch(None, |b| {
        let mut summary = Summary {
            secrets_skipped: skipped,
            ..Summary::default()
        };
        let mut entries = b.entries.clone();
        for target in MemoryTarget::ALL.iter().copied() {
            let ours: Vec<&Item> = kept
                .iter()
                .copied()
                .filter(|i| i.target == target)
                .collect();
            if ours.is_empty() {
                continue;
            }
            let ctx = write::context(config, target, MemoryOrigin::Import, None);
            import_target(&mut entries, &ours, &ctx, &mut summary).map_err(refused)?;
        }
        b.entries = entries;
        Ok(summary)
    })
}

fn import_target(
    entries: &mut Vec<MemoryEntry>,
    items: &[&Item],
    ctx: &Context,
    summary: &mut Summary,
) -> Result<(), String> {
    let before = budget::core_chars(entries, ctx.target);
    let mut ops = Vec::new();
    for item in items {
        ops.extend(item_ops(entries, item, ctx, summary));
    }
    let applied = batch::apply_unchecked(entries, &ops, ctx)?;
    let mut patched = BTreeSet::new();
    for r in &applied.results {
        match r.action {
            Action::Add => count(summary, Action::Add, r.duplicate),
            Action::Patch => {
                patched.insert(r.id.clone());
            }
            _ => {}
        }
    }
    summary.updated += patched.len();
    batch::check_limits(entries, ctx, before)?;
    block_threats(entries, ctx, summary);
    Ok(())
}

/// What importing `item` takes: restoring an unknown id in place (no
/// operation), patching a known one, or an add.
fn item_ops(
    entries: &mut Vec<MemoryEntry>,
    item: &Item,
    ctx: &Context,
    summary: &mut Summary,
) -> Vec<Op> {
    let content = item.content.trim();
    let Some(id) = &item.id else {
        return vec![writing(Action::Add, None, content, item.kind, item.tier)];
    };
    let Some(e) = entries.iter().find(|e| e.id == *id) else {
        if crate::id::is_entry(id) && !is_duplicate(entries, item) {
            entries.push(restored(item, id, ctx));
            summary.added += 1;
            return Vec::new();
        }
        return vec![writing(Action::Add, None, content, item.kind, item.tier)];
    };
    let same = e.content == content
        && e.kind == item.kind
        && e.tier == item.tier
        && e.pinned == item.pinned;
    if same {
        return Vec::new();
    }
    vec![
        writing(Action::Patch, Some(id), content, item.kind, item.tier),
        on(
            if item.pinned {
                Action::Pin
            } else {
                Action::Unpin
            },
            id,
        ),
    ]
}

fn is_duplicate(entries: &[MemoryEntry], item: &Item) -> bool {
    entries.iter().any(|e| {
        e.target == item.target
            && e.status == MemoryStatus::Active
            && e.content.trim() == item.content.trim()
    })
}

fn restored(item: &Item, id: &str, ctx: &Context) -> MemoryEntry {
    MemoryEntry {
        id: id.to_owned(),
        target: item.target,
        kind: item.kind,
        tier: item.tier,
        content: item.content.trim().to_owned(),
        pinned: item.pinned,
        status: MemoryStatus::Active,
        expires_at: None,
        created_at: ctx.now,
        updated_at: ctx.now,
        last_recalled_at: None,
        recall_count: 0,
        origin: MemoryOrigin::Import,
        origin_session: None,
    }
}

/// Marks entries this import wrote that match a threat pattern `blocked`.
fn block_threats(entries: &mut [MemoryEntry], ctx: &Context, summary: &mut Summary) {
    for e in entries.iter_mut().filter(|e| {
        e.target == ctx.target && e.updated_at == ctx.now && e.status == MemoryStatus::Active
    }) {
        if !scan::threats(&e.content).is_empty() {
            e.status = MemoryStatus::Blocked;
            summary.blocked.push(e.id.clone());
        }
    }
}
