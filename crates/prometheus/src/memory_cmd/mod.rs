//! `prometheus memory` (spec: memory, Memory CLI and editing; Store enable
//! flags and tool availability; Write approval and pending changes; Threat
//! scanning of memory content). Changes made here record `origin = user`
//! (`import` for imports) and reach the system prompt from the next
//! session.

pub mod args;
mod edit;

use std::fmt::Write as _;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_memory::{markdown, slash, write};
use pmt_store::{
    MemoryEntry, MemoryKind, MemoryOrigin, MemoryStatus, MemoryTarget, MemoryTier, Store,
};
use serde_json::{Value, json};

use crate::context::Context;
use crate::output::{json, print_raw};
use crate::prompt;
use args::{FormatArg, KindArg, MemoryCommand, ResetTarget, TargetArg, TierArg};

/// What every subcommand works on.
pub(crate) struct Env {
    pub(crate) scope: ProfileScope,
    pub(crate) store: Arc<Store>,
    pub(crate) json: bool,
}

impl Env {
    fn config(&self) -> &pmt_config::schema::MemoryConfig {
        &self.scope.config().memory
    }
}

/// Runs one `memory` subcommand and returns its exit code.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &MemoryCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let store = Arc::new(Store::open(&scope)?);
    let env = Env {
        scope,
        store,
        json: ctx.json,
    };
    match command {
        MemoryCommand::Status => status(&env),
        MemoryCommand::List {
            target,
            kind,
            tier,
            all,
        } => list(&env, *target, *kind, *tier, *all),
        MemoryCommand::Show { id } => show(&env, id),
        MemoryCommand::Add {
            content,
            target,
            kind,
            tier,
        } => add(&env, content, *target, *kind, *tier),
        MemoryCommand::Rm { id } => change(&env, id, pmt_memory::ops::Action::Remove),
        MemoryCommand::Pin { id } => change(&env, id, pmt_memory::ops::Action::Pin),
        MemoryCommand::Unpin { id } => change(&env, id, pmt_memory::ops::Action::Unpin),
        MemoryCommand::Edit { target } => edit::run(&env, ctx.launch, &targets(*target)),
        MemoryCommand::Export {
            format,
            target,
            output,
        } => export(&env, *format, *target, output.as_deref()),
        MemoryCommand::Import { file, target, tier } => import(&env, file, *target, *tier),
        MemoryCommand::Pending => reply(&env, "pending"),
        MemoryCommand::Approve { id } => reply(&env, &format!("approve {id}")),
        MemoryCommand::Reject { id } => reply(&env, &format!("reject {id}")),
        MemoryCommand::Reset { target, yes } => reset(&env, *target, *yes),
    }
}

const fn target(arg: TargetArg) -> MemoryTarget {
    match arg {
        TargetArg::Memory => MemoryTarget::Memory,
        TargetArg::User => MemoryTarget::User,
    }
}

fn targets(arg: Option<TargetArg>) -> Vec<MemoryTarget> {
    arg.map_or_else(|| MemoryTarget::ALL.to_vec(), |t| vec![target(t)])
}

const fn kind(arg: KindArg) -> MemoryKind {
    match arg {
        KindArg::Fact => MemoryKind::Fact,
        KindArg::Preference => MemoryKind::Preference,
        KindArg::Instruction => MemoryKind::Instruction,
        KindArg::Project => MemoryKind::Project,
        KindArg::Person => MemoryKind::Person,
        KindArg::Event => MemoryKind::Event,
    }
}

const fn tier(arg: TierArg) -> MemoryTier {
    match arg {
        TierArg::Core => MemoryTier::Core,
        TierArg::Archival => MemoryTier::Archival,
    }
}

/// The JSON form of an entry.
pub(crate) fn entry_json(e: &MemoryEntry) -> Value {
    json!({
        "id": e.id,
        "target": e.target,
        "kind": e.kind,
        "tier": e.tier,
        "content": e.content,
        "pinned": e.pinned,
        "status": e.status,
        "expires_at": e.expires_at.map(|t| t.to_string()),
        "created_at": e.created_at.to_string(),
        "updated_at": e.updated_at.to_string(),
        "last_recalled_at": e.last_recalled_at.map(|t| t.to_string()),
        "recall_count": e.recall_count,
        "origin": e.origin,
        "origin_session": e.origin_session,
    })
}

fn status(env: &Env) -> Result<u8, Error> {
    let status = pmt_memory::status::status(&env.store, env.config())?;
    if env.json {
        return json(&status);
    }
    print_raw(&format!("{}\n", pmt_memory::status::render(&status)))
}

fn list(
    env: &Env,
    t: Option<TargetArg>,
    k: Option<KindArg>,
    r: Option<TierArg>,
    all: bool,
) -> Result<u8, Error> {
    let entries: Vec<MemoryEntry> = env
        .store
        .memory_entries(t.map(target))?
        .into_iter()
        .filter(|e| k.is_none_or(|k| e.kind == kind(k)) && r.is_none_or(|r| e.tier == tier(r)))
        .filter(|e| all || e.status != MemoryStatus::Dormant)
        .collect();
    if env.json {
        return json(&entries.iter().map(entry_json).collect::<Vec<_>>());
    }
    if entries.is_empty() {
        eprintln!("no memory entries");
        return Ok(0);
    }
    let mut text = String::new();
    for e in &entries {
        let flags = match (e.pinned, e.status) {
            (_, MemoryStatus::Blocked) => " [blocked]",
            (_, MemoryStatus::Dormant) => " [dormant]",
            (true, _) => " [pinned]",
            (false, _) => "",
        };
        let preview: String = e
            .content
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(70)
            .collect();
        let _ = writeln!(
            text,
            "{}  {:<6} {:<8} {:<11} {preview}{flags}",
            e.id, e.target, e.tier, e.kind
        );
    }
    print_raw(&text)
}

fn find(env: &Env, id: &str) -> Result<MemoryEntry, Error> {
    env.store.memory_entry(id)?.ok_or_else(|| {
        Error::new(&codes::MEMORY_NOT_FOUND, format!("no memory entry '{id}'"))
            .with_hint("prometheus memory list --all")
    })
}

fn show(env: &Env, id: &str) -> Result<u8, Error> {
    let e = find(env, id)?;
    if env.json {
        return json(&entry_json(&e));
    }
    let mut text = format!(
        "id:       {}\ntarget:   {}\nkind:     {}\ntier:     {}\nstatus:   {}\npinned:   {}\norigin:   {}\ncreated:  {}\nupdated:  {}\nrecalled: {} times\n",
        e.id,
        e.target,
        e.kind,
        e.tier,
        e.status,
        e.pinned,
        e.origin,
        e.created_at,
        e.updated_at,
        e.recall_count
    );
    if let Some(at) = e.expires_at {
        let _ = writeln!(text, "expires:  {at}");
    }
    if e.status == MemoryStatus::Blocked {
        let rules = pmt_memory::scan::threats(&e.content).join(", ");
        let _ = writeln!(
            text,
            "blocked:  matches {rules}; the model sees [BLOCKED: ...] (remove it with prometheus memory rm {})",
            e.id
        );
    }
    let _ = write!(text, "\n{}\n", e.content);
    print_raw(&text)
}

/// Commits `ops` on `t` as the user.
fn commit(
    env: &Env,
    t: MemoryTarget,
    ops: &[pmt_memory::ops::Op],
) -> Result<write::Written, Error> {
    let ctx = write::context(env.config(), t, MemoryOrigin::User, None);
    write::write(&env.store, &ctx, ops, &[])
}

fn add(env: &Env, content: &str, t: TargetArg, k: KindArg, r: TierArg) -> Result<u8, Error> {
    let redactor = env.scope.secrets().redactor()?;
    let content = content.trim();
    if content.is_empty() {
        return Err(Error::new(&codes::MEMORY_INVALID, "the entry is empty"));
    }
    pmt_memory::scan::check(content, &redactor)
        .map_err(|m| Error::new(&codes::MEMORY_BLOCKED, m))?;
    let mut op = pmt_memory::ops::Op::new(pmt_memory::ops::Action::Add);
    op.content = Some(content.to_owned());
    op.kind = Some(kind(k));
    op.tier = Some(tier(r));
    let written = commit(env, target(t), &[op])?;
    let result = &written.results[0];
    if env.json {
        return json(
            &json!({"id": result.id, "duplicate": result.duplicate, "usage": written.usage}),
        );
    }
    if result.duplicate {
        eprintln!("already remembered as {}", result.id);
    } else {
        eprintln!("added {} (from the next session)", result.id);
    }
    print_raw(&format!("{}\n", result.id))
}

fn change(env: &Env, id: &str, action: pmt_memory::ops::Action) -> Result<u8, Error> {
    let entry = find(env, id)?;
    let mut op = pmt_memory::ops::Op::new(action);
    op.id = Some(entry.id.clone());
    commit(env, entry.target, &[op])?;
    if env.json {
        return json(&json!({"id": entry.id, "action": action.as_str()}));
    }
    eprintln!("{} {} (from the next session)", past(action), entry.id);
    Ok(0)
}

const fn past(action: pmt_memory::ops::Action) -> &'static str {
    match action {
        pmt_memory::ops::Action::Remove => "removed",
        pmt_memory::ops::Action::Pin => "pinned",
        pmt_memory::ops::Action::Unpin => "unpinned",
        pmt_memory::ops::Action::Add => "added",
        pmt_memory::ops::Action::Patch => "patched",
    }
}

fn export(
    env: &Env,
    format: FormatArg,
    t: Option<TargetArg>,
    output: Option<&std::path::Path>,
) -> Result<u8, Error> {
    let wanted = targets(t);
    let entries: Vec<MemoryEntry> = env
        .store
        .memory_entries(None)?
        .into_iter()
        .filter(|e| wanted.contains(&e.target))
        .collect();
    let text = match format {
        FormatArg::Md => markdown::render(&entries, &wanted),
        FormatArg::Jsonl => entries.iter().fold(String::new(), |mut out, e| {
            let _ = writeln!(out, "{}", entry_json(e));
            out
        }),
    };
    match output {
        None => print_raw(&text),
        Some(path) => {
            pmt_core::private_fs::write_atomic(path, text.as_bytes())
                .map_err(|e| Error::new(&codes::HOME_IO, format!("{}: {e}", path.display())))?;
            eprintln!("exported {} entries to {}", entries.len(), path.display());
            Ok(0)
        }
    }
}

fn import(
    env: &Env,
    file: &std::path::Path,
    t: Option<TargetArg>,
    r: TierArg,
) -> Result<u8, Error> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| Error::new(&codes::MEMORY_INVALID, format!("{}: {e}", file.display())))?;
    let items = pmt_memory::manage::read(file, &text, t.map(target), tier(r))?;
    let redactor = env.scope.secrets().redactor()?;
    let summary = pmt_memory::manage::import(&env.store, env.config(), &items, &redactor)?;
    if env.json {
        return json(&summary);
    }
    eprintln!(
        "imported {} new, {} updated, {} duplicates skipped",
        summary.added, summary.updated, summary.duplicates
    );
    if summary.secrets_skipped > 0 {
        eprintln!(
            "skipped {} entries holding credentials",
            summary.secrets_skipped
        );
    }
    for id in &summary.blocked {
        eprintln!(
            "kept {id} as blocked: it matches a prompt-injection pattern (inspect with prometheus memory show {id})"
        );
    }
    Ok(0)
}

fn reply(env: &Env, args: &str) -> Result<u8, Error> {
    if env.json && args == "pending" {
        let pending = env.store.memory_pending()?;
        return json(&pending.iter().map(write::pending_json).collect::<Vec<_>>());
    }
    let reply = slash::run(&env.store, env.config(), args)?;
    print_raw(&format!("{}\n", reply.text))
}

fn reset(env: &Env, which: ResetTarget, yes: bool) -> Result<u8, Error> {
    let wanted: Vec<MemoryTarget> = match which {
        ResetTarget::All => MemoryTarget::ALL.to_vec(),
        ResetTarget::Memory => vec![MemoryTarget::Memory],
        ResetTarget::User => vec![MemoryTarget::User],
    };
    let names = wanted
        .iter()
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(" and ");
    if !yes {
        match prompt::confirm_typed(&format!("delete every {names} memory entry"), "delete")? {
            None => {
                eprintln!("error: confirmation needed; rerun with --yes");
                return Ok(prompt::NEEDS_CONFIRMATION);
            }
            Some(false) => return Ok(1),
            Some(true) => {}
        }
    }
    let removed = env.store.memory_batch(None, |b| {
        let before = b.entries.len();
        b.entries.retain(|e| !wanted.contains(&e.target));
        Ok(before - b.entries.len())
    })?;
    if env.json {
        return json(&json!({"removed": removed}));
    }
    eprintln!("removed {removed} {names} entries");
    Ok(0)
}
