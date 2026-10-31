//! `prometheus sessions` (spec: session-storage, Session management CLI;
//! Full-text session search; Export formats; Deletion semantics;
//! Retention; Rewind is a soft delete).

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::id::{MessageId, SessionId};
use pmt_store::{
    Archived, ListOptions, Rewound, SearchOptions, SearchResults, SearchSort, Session, Store,
    UsageScope, UsageTotals,
};
use serde_json::{Value, json};

use crate::cli::{ExportArgs, SearchArgs, SessionsCommand, SortArg};
use crate::context::Context;
use crate::output::{document, json as print_json, print, print_error, print_raw};
use crate::prompt;
use crate::session_export::{self, ExportOptions, session_json};

/// Runs one `sessions` subcommand on the selected profile's store.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &SessionsCommand, ctx: &Context<'_>, ui: crate::chat::Ui) -> Result<u8, Error> {
    if let SessionsCommand::Browse { source, all } = command {
        return crate::sessions_browse::run(*source, *all, ctx, ui);
    }
    let scope = ctx.scope()?;
    let store = Store::open(&scope)?;
    let session = |reference: &str| store.resolve_session(reference);
    match command {
        SessionsCommand::List {
            limit,
            all,
            source,
            pinned,
        } => {
            let options = ListOptions {
                limit: *limit,
                all: *all,
                sources: source.clone(),
                pinned_only: *pinned,
            };
            list(ctx, &store.list_sessions(&options)?)
        }
        SessionsCommand::Browse { .. } => unreachable!("handled above"),
        SessionsCommand::Show { session: r } => show(ctx, &store, session(r)?),
        SessionsCommand::Search(args) => search(ctx, &store, args),
        SessionsCommand::Rename { session: r, title } => {
            let id = session(r)?;
            store.set_title(id, Some(title), pmt_store::TitleSource::User)?;
            changed(ctx, &store.session(id)?, "renamed")
        }
        SessionsCommand::Pin { session: r } => {
            changed(ctx, &store.set_pinned(session(r)?, true)?, "pinned")
        }
        SessionsCommand::Unpin { session: r } => {
            changed(ctx, &store.set_pinned(session(r)?, false)?, "unpinned")
        }
        SessionsCommand::Archive { session: r } => changed(
            ctx,
            &store.set_archived(session(r)?, Archived::User)?,
            "archived",
        ),
        SessionsCommand::Unarchive { session: r } => changed(
            ctx,
            &store.set_archived(session(r)?, Archived::None)?,
            "unarchived",
        ),
        SessionsCommand::Tree { session: r } => tree(ctx, &store, session(r)?),
        SessionsCommand::Stats { session: r } => stats(ctx, &store, session(r)?),
        SessionsCommand::Export(args) => export(ctx, &scope, &store, args),
        SessionsCommand::Delete {
            session: r,
            with_lineage,
            yes,
        } => delete(ctx, &store, session(r)?, *with_lineage, *yes),
        SessionsCommand::Prune {
            older_than,
            dry_run,
            yes,
        } => prune(ctx, &store, *older_than, *dry_run, *yes),
        SessionsCommand::Rewind {
            session: r,
            to,
            restore,
        } => rewind(ctx, &store, session(r)?, to.as_deref(), restore.as_deref()),
        SessionsCommand::Repair { indexes } => repair(ctx, &store, *indexes),
        SessionsCommand::Vacuum => {
            store.vacuum()?;
            eprintln!("vacuumed {}", store.location().database().display());
            Ok(0)
        }
    }
}

fn day(ts: jiff::Timestamp) -> String {
    ts.strftime("%Y-%m-%d %H:%M").to_string()
}

/// One line per session: id, last activity, source, messages, title.
fn render_list(sessions: &[Session]) -> String {
    sessions.iter().fold(String::new(), |mut text, s| {
        let _ = writeln!(
            text,
            "{}  {}  {:<10}  {:>5} msgs  {}",
            s.id,
            day(s.last_activity_at),
            tagged_source(s),
            s.message_count,
            s.title.as_deref().unwrap_or("-")
        );
        text
    })
}

fn list(ctx: &Context<'_>, sessions: &[Session]) -> Result<u8, Error> {
    if ctx.json {
        let sessions: Vec<Value> = sessions.iter().map(session_json).collect();
        return document("sessions list", &json!({ "sessions": sessions }));
    }
    if sessions.is_empty() {
        eprintln!("no sessions");
        return Ok(0);
    }
    print_raw(&render_list(sessions))
}

fn show(ctx: &Context<'_>, store: &Store, id: SessionId) -> Result<u8, Error> {
    let session = store.session(id)?;
    let lineage: Vec<String> = store.lineage(id)?.iter().map(ToString::to_string).collect();
    let flags = store.launch_flags(id)?;
    let flag = |name: &str| flags.iter().any(|f| f == name);
    if ctx.json {
        let mut body = session_json(&session);
        body["lineage"] = json!(lineage);
        body["ignore_user_config"] = json!(flag("ignore_user_config"));
        body["ignore_rules"] = json!(flag("ignore_rules"));
        return document("sessions show", &json!({ "session": body }));
    }
    let mut text = String::new();
    let field = |text: &mut String, name: &str, value: &dyn std::fmt::Display| {
        let _ = writeln!(text, "{name:<14}{value}");
    };
    field(&mut text, "id", &session.id);
    field(&mut text, "title", &session.title.as_deref().unwrap_or("-"));
    field(&mut text, "source", &tagged_source(&session));
    field(&mut text, "started", &day(session.started_at));
    field(&mut text, "last activity", &day(session.last_activity_at));
    let ended = session
        .end_reason
        .map_or("-".to_owned(), |r| r.as_str().to_owned());
    field(&mut text, "ended", &ended);
    field(&mut text, "messages", &session.message_count);
    field(&mut text, "pinned", &session.pinned);
    field(&mut text, "archived", &session.archived);
    field(&mut text, "lineage", &lineage.join(" -> "));
    for name in &flags {
        let _ = writeln!(text, "{name} = true");
    }
    print_raw(&text)
}

fn tagged_source(session: &Session) -> String {
    match &session.source_tag {
        Some(tag) => format!("{} ({tag})", session.source),
        None => session.source.to_string(),
    }
}

fn changed(ctx: &Context<'_>, session: &Session, what: &str) -> Result<u8, Error> {
    if ctx.json {
        return print_json(&json!({ "session": session_json(session) }));
    }
    eprintln!("{what} {}", session.id);
    Ok(0)
}

fn search(ctx: &Context<'_>, store: &Store, args: &SearchArgs) -> Result<u8, Error> {
    let options = SearchOptions {
        limit: args.limit,
        offset: args.offset,
        sort: match args.sort {
            SortArg::Rank => SearchSort::Rank,
            SortArg::Newest => SearchSort::Newest,
            SortArg::Oldest => SearchSort::Oldest,
        },
        sources: args.source.clone(),
        exclude_sources: args.exclude_source.clone(),
        roles: args.role.clone(),
        after: args.after,
        before: args.before,
        session_id: args
            .session
            .as_deref()
            .map(|r| store.resolve_session(r))
            .transpose()?,
        exclude_session_ids: Vec::new(),
        pinned: args.pinned.then_some(true),
        include_archived: !args.no_archived,
        include_inactive: args.include_inactive,
    };
    let results = store.search(&args.query, &options)?;
    if ctx.json {
        return document("sessions search", &search_json(&results));
    }
    if results.hits.is_empty() {
        eprintln!("no matches");
        return Ok(0);
    }
    let text = results.hits.iter().fold(String::new(), |mut text, h| {
        let _ = writeln!(
            text,
            "{}  {}  {}  {}\n  {}",
            h.session_id,
            h.message_id,
            h.role,
            day(h.created_at),
            h.snippet
        );
        text
    });
    print_raw(&text)
}

fn search_json(results: &SearchResults) -> Value {
    let hits: Vec<Value> = results
        .hits
        .iter()
        .map(|h| {
            json!({
                "session_id": h.session_id.to_string(),
                "session_title": h.session_title,
                "message_id": h.message_id.to_string(),
                "role": h.role.as_str(),
                "created_at": h.created_at.to_string(),
                "snippet": h.snippet,
                "score": h.score,
            })
        })
        .collect();
    let mode = match results.mode {
        pmt_store::SearchMode::Word => "word",
        pmt_store::SearchMode::Trigram => "trigram",
        pmt_store::SearchMode::Scan => "scan",
    };
    json!({ "mode": mode, "hits": hits })
}

fn tree(ctx: &Context<'_>, store: &Store, id: SessionId) -> Result<u8, Error> {
    let nodes = store.tree(id)?;
    if ctx.json {
        let nodes: Vec<Value> = nodes
            .iter()
            .map(|n| json!({"id": n.id.to_string(), "parent": n.parent.to_string(), "kind": n.kind.as_str(), "depth": n.depth}))
            .collect();
        return document(
            "sessions tree",
            &json!({ "root": id.to_string(), "nodes": nodes }),
        );
    }
    let mut text = format!("{id}\n");
    for node in &nodes {
        let indent = "  ".repeat(usize::try_from(node.depth).unwrap_or(0));
        let _ = writeln!(text, "{indent}{} ({})", node.id, node.kind);
    }
    print_raw(&text)
}

fn totals_json(totals: &UsageTotals) -> Value {
    json!({
        "calls": totals.calls,
        "input_tokens": totals.tokens.input,
        "output_tokens": totals.tokens.output,
        "cache_read_tokens": totals.tokens.cache_read,
        "cache_write_tokens": totals.tokens.cache_write,
        "reasoning_tokens": totals.tokens.reasoning,
        "cost_usd": totals.cost_usd,
        "unpriced_calls": totals.unpriced_calls,
    })
}

fn render_totals(label: &str, totals: &UsageTotals) -> String {
    let cost = pmt_obs::usage::format_total_cost(totals);
    format!(
        "{label:<9}{} calls  in {}  out {}  cache r/w {}/{}  reasoning {}  cost {cost} ({} unpriced)\n",
        totals.calls,
        totals.tokens.input,
        totals.tokens.output,
        totals.tokens.cache_read,
        totals.tokens.cache_write,
        totals.tokens.reasoning,
        totals.unpriced_calls
    )
}

fn stats(ctx: &Context<'_>, store: &Store, id: SessionId) -> Result<u8, Error> {
    let session = store.usage_totals(UsageScope::Session(id))?;
    let lineage = store.usage_totals(UsageScope::Lineage(id))?;
    let active = store.active_lineage_messages(id)?;
    if ctx.json {
        return document(
            "sessions stats",
            &json!({
                "session_id": id.to_string(),
                "active_lineage_messages": active,
                "session": totals_json(&session),
                "lineage": totals_json(&lineage),
            }),
        );
    }
    let text = format!(
        "{id}\nactive messages in lineage: {active}\n{}{}",
        render_totals("session", &session),
        render_totals("lineage", &lineage)
    );
    print_raw(&text)
}

/// Known secret values of the profile (store and launch environment) for
/// a redacted export.
pub(crate) fn redactor(scope: &ProfileScope) -> Result<pmt_core::secret::Redactor, Error> {
    scope.secrets().redactor()
}

fn export(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    store: &Store,
    args: &ExportArgs,
) -> Result<u8, Error> {
    let ids: Vec<SessionId> = if args.all {
        let all = ListOptions {
            all: true,
            ..ListOptions::default()
        };
        store
            .list_sessions(&all)?
            .iter()
            .rev()
            .map(|s| s.id)
            .collect()
    } else {
        args.sessions
            .iter()
            .map(|r| store.resolve_session(r))
            .collect::<Result<_, _>>()?
    };
    let redactor = args.redact.then(|| redactor(scope)).transpose()?;
    let options = ExportOptions {
        format: args.format,
        include_inactive: args.include_inactive,
        redactor: redactor.as_ref(),
    };
    let Some(path) = &args.output else {
        session_export::export(store, &ids, options, &mut std::io::stdout().lock())?;
        return Ok(0);
    };
    let mut file = match pmt_core::private_fs::new_file_options().open(path) {
        Ok(file) => file,
        Err(e) => {
            print_error(&cannot_create(path, &e), ctx.json);
            return Ok(CANNOT_CREATE);
        }
    };
    session_export::export(store, &ids, options, &mut file)?;
    file.flush().map_err(|e| cannot_create(path, &e))?;
    if ctx.json {
        return print_json(&json!({"path": path, "sessions": ids.len()}));
    }
    eprintln!("exported {} session(s) to {}", ids.len(), path.display());
    Ok(0)
}

/// Exit status when the export file cannot be created (shared exit codes).
pub const CANNOT_CREATE: u8 = 73;

fn cannot_create(path: &std::path::Path, e: &std::io::Error) -> Error {
    Error::new(
        &codes::OUTPUT_IO,
        format!("cannot write {}: {e}", path.display()),
    )
}

fn confirm(what: &str, yes: bool) -> Result<Option<u8>, Error> {
    if yes {
        return Ok(None);
    }
    match prompt::confirm_typed(what, "delete")? {
        None => {
            eprintln!("error: confirmation needed; rerun with --yes");
            Ok(Some(prompt::NEEDS_CONFIRMATION))
        }
        Some(false) => {
            eprintln!("nothing deleted");
            Ok(Some(1))
        }
        Some(true) => Ok(None),
    }
}

fn delete(
    ctx: &Context<'_>,
    store: &Store,
    id: SessionId,
    with_lineage: bool,
    yes: bool,
) -> Result<u8, Error> {
    if let Some(code) = confirm(&format!("delete session {id} and its messages"), yes)? {
        return Ok(code);
    }
    let deleted = store.delete_session(id, with_lineage)?;
    let ids = |list: &[SessionId]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
    if ctx.json {
        return print_json(&json!({
            "deleted": ids(&deleted.sessions),
            "reparented": ids(&deleted.reparented),
            "blobs": deleted.blobs,
        }));
    }
    for session in &deleted.sessions {
        eprintln!("deleted {session}");
    }
    for kept in &deleted.reparented {
        eprintln!("kept {kept} (parent deleted)");
    }
    Ok(0)
}

fn prune(
    ctx: &Context<'_>,
    store: &Store,
    days: u64,
    dry_run: bool,
    yes: bool,
) -> Result<u8, Error> {
    let older_than = Duration::from_secs(days.saturating_mul(86_400));
    let would = store.prune(older_than, true)?;
    let ids = |list: &[SessionId]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
    if dry_run {
        if ctx.json {
            return document(
                "sessions prune",
                &json!({"dry_run": true, "count": would.len(), "sessions": ids(&would)}),
            );
        }
        return print(&format!("would delete {} session(s)", would.len()));
    }
    if !would.is_empty()
        && let Some(code) = confirm(
            &format!(
                "delete {} session(s) inactive for over {days} days",
                would.len()
            ),
            yes,
        )?
    {
        return Ok(code);
    }
    let pruned = store.prune(older_than, false)?;
    if ctx.json {
        return document(
            "sessions prune",
            &json!({"dry_run": false, "count": pruned.len(), "sessions": ids(&pruned)}),
        );
    }
    print(&format!("deleted {} session(s)", pruned.len()))
}

fn rewind(
    ctx: &Context<'_>,
    store: &Store,
    id: SessionId,
    to: Option<&str>,
    restore: Option<&str>,
) -> Result<u8, Error> {
    let rewound: Rewound = match (to, restore) {
        (_, Some(message)) => store.restore_rewind(id, MessageId::parse(message)?, None)?,
        (Some(message), None) => store.rewind(id, MessageId::parse(message)?, None, None)?,
        (None, None) => store.undo_last_turn(id, None)?,
    };
    let verb = if restore.is_some() {
        "restored"
    } else {
        "rewound"
    };
    if ctx.json {
        let messages: Vec<String> = rewound.messages.iter().map(ToString::to_string).collect();
        return print_json(
            &json!({"session_id": id.to_string(), "batch": rewound.batch, verb: messages}),
        );
    }
    eprintln!("{verb} {} message(s) of {id}", rewound.messages.len());
    Ok(0)
}

fn repair(ctx: &Context<'_>, store: &Store, rebuild_indexes: bool) -> Result<u8, Error> {
    if !rebuild_indexes {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "name what to repair: prometheus sessions repair --indexes",
        ));
    }
    let indexed = store.repair_indexes()?;
    if ctx.json {
        return print_json(&json!({"indexed_messages": indexed}));
    }
    eprintln!("rebuilt the search indexes ({indexed} messages)");
    Ok(0)
}
