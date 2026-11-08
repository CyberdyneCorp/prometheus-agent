//! `prometheus sessions recover|journal-mode|refresh-prompts|retitle`
//! (spec: session-storage, Offline recovery and journal mode; Prompt
//! refresh and retitling).

use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::path::Path;

use pmt_config::ProfileScope;
use pmt_config::schema::JournalMode;
use pmt_core::error::Error;
use pmt_core::runtime::{Role, WriterLock};
use pmt_store::{ListOptions, Session, Store, TitleSource, TitleWrite, Visibility};
use serde_json::json;

use super::args::JournalModeArg;
use crate::context::Context;
use crate::output::json as print_json;

/// The home's writer lock: refuses (exit 75, naming the holder's pid)
/// while a daemon or a chat has the store open.
fn closed(scope: &ProfileScope) -> Result<WriterLock, Error> {
    WriterLock::acquire(scope.home(), Role::Chat, vec![scope.name().to_string()])
}

/// `sessions recover`.
pub(super) fn recover(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    output: Option<&Path>,
    yes: bool,
) -> Result<u8, Error> {
    let _lock = closed(scope)?;
    let target = output.map_or_else(
        || pmt_store::recover::default_output(scope),
        Path::to_path_buf,
    );
    let report = pmt_store::recover::recover(scope, &target)?;
    let mut text = format!("recovered store written to {}\n", target.display());
    for (table, rows) in &report.recovered {
        let lost = report.lost.get(table).copied().unwrap_or(0);
        if *rows > 0 || lost > 0 {
            let _ = writeln!(text, "  {table}: {rows} recovered, {lost} lost");
        }
    }
    eprint!("{text}");
    let swap = output.is_none()
        && (yes
            || (std::io::stdin().is_terminal()
                && crate::prompt::confirm("Replace state.db with the recovered store?")?));
    let kept = if swap {
        let kept = pmt_store::recover::swap_in(scope, &target)?;
        eprintln!(
            "state.db replaced; the original is kept as {}",
            kept.display()
        );
        Some(kept)
    } else {
        if output.is_none() {
            eprintln!("state.db unchanged; rerun with --yes to swap the recovered store in");
        }
        None
    };
    if ctx.json {
        return print_json(&json!({"report": report, "swapped": kept.is_some(), "original": kept}));
    }
    Ok(0)
}

/// `sessions journal-mode`.
pub(super) fn journal_mode(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    mode: JournalModeArg,
) -> Result<u8, Error> {
    let _lock = closed(scope)?;
    let (mode, name) = match mode {
        JournalModeArg::Wal => (JournalMode::Wal, "wal"),
        JournalModeArg::Delete => (JournalMode::Delete, "delete"),
    };
    pmt_store::recover::set_journal_mode(scope, mode)?;
    if ctx.json {
        return print_json(&json!({"journal_mode": name}));
    }
    eprintln!(
        "state.db now uses journal mode {name} (recorded in the store, applied on every open)"
    );
    Ok(0)
}

fn pick(store: &Store, refs: &[String], all: bool) -> Result<Vec<Session>, Error> {
    if all {
        return store.list_sessions(&ListOptions::default());
    }
    refs.iter()
        .map(|r| store.resolve_session(r).and_then(|id| store.session(id)))
        .collect()
}

/// `sessions refresh-prompts`.
pub(super) fn refresh_prompts(
    ctx: &Context<'_>,
    store: &Store,
    refs: &[String],
    all: bool,
) -> Result<u8, Error> {
    let mut marked = Vec::new();
    for session in pick(store, refs, all)? {
        // Only a session with a stored prompt has one to rebuild; the
        // others build theirs fresh at their first turn anyway.
        if store.session_prompt(session.id)?.is_some() {
            store.mark_prompt_refresh(session.id)?;
            marked.push(session.id.to_string());
        }
    }
    if ctx.json {
        return print_json(&json!({"marked": marked}));
    }
    eprintln!(
        "{} session(s) rebuild their system prompt at their next turn",
        marked.len()
    );
    Ok(0)
}

/// The first messages of `session` as the title model sees them.
fn conversation(store: &Store, session: &Session) -> Result<String, Error> {
    let mut text = String::new();
    for m in store
        .messages(session.id, Visibility::Active)?
        .iter()
        .filter(|m| matches!(m.role, pmt_store::Role::User | pmt_store::Role::Assistant))
        .take(6)
    {
        let body: String = m
            .content
            .as_deref()
            .unwrap_or_default()
            .chars()
            .take(500)
            .collect();
        let _ = writeln!(text, "{}: {body}", m.role);
    }
    Ok(text)
}

/// `sessions retitle`.
pub(super) fn retitle(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    store: &Store,
    refs: &[String],
    (all, auto_only): (bool, bool),
) -> Result<u8, Error> {
    let chosen: Vec<Session> = pick(store, refs, all || auto_only)?
        .into_iter()
        .filter(|s| s.title_source != Some(TitleSource::User))
        .filter(|s| !auto_only || s.title.is_some())
        .collect();
    let user_home = ctx.launch.user_home().map(std::path::PathBuf::as_path);
    let models = crate::chat::session::model::Models::open(
        scope,
        &std::sync::Arc::new(Store::open(scope)?),
        user_home,
        &crate::chat::session::model::RouteFlags::default(),
        None,
    )?;
    let mut retitled = Vec::new();
    for session in chosen {
        let text = conversation(store, &session)?;
        if text.is_empty() {
            continue;
        }
        let asked = crate::model_cmd::block_on(models.ask_title(session.id, &text))??;
        let Some(title) = asked else {
            eprintln!("no title route answers; nothing retitled");
            break;
        };
        if store.retitle(session.id, &title)? == TitleWrite::Written {
            retitled.push(
                json!({"id": session.id.to_string(), "title": store.session(session.id)?.title}),
            );
        }
    }
    if ctx.json {
        return print_json(&json!({"retitled": retitled}));
    }
    eprintln!("{} session(s) retitled", retitled.len());
    Ok(0)
}
