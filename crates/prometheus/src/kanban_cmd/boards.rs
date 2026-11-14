//! `kanban boards`, `kanban notify` and `kanban gc` (spec: kanban, Board
//! slugs and selection; Board export and import; Notifications through
//! the owning profile).

use std::fmt::Write as _;

use pmt_core::error::Error;
use pmt_kanban::notify::NewSubscription;
use pmt_kanban::{Filter, Status, Workspace, transfer};
use serde_json::json;

use super::{BoardsCommand, Env, NotifyCommand};
use crate::output::{document, print, print_raw};

/// Tick summaries older than this are pruned by `gc`.
const TICK_RETENTION_MS: i64 = 7 * 24 * 3_600_000;

/// `kanban boards ...`.
pub(super) fn boards(env: &Env<'_>, command: &BoardsCommand) -> Result<u8, Error> {
    let boards = &env.boards;
    match command {
        BoardsCommand::List => list(env),
        BoardsCommand::Create { slug, name } => {
            let slug = boards.create(slug, name.as_deref())?;
            print(&format!("created board {slug}"))
        }
        BoardsCommand::Rename { from, to } => {
            let to = boards.rename(from, to)?;
            print(&format!("renamed board {from} to {to}"))
        }
        BoardsCommand::Archive { slug } => {
            boards.set_archived(&pmt_kanban::boards::slug(slug)?, true)?;
            print(&format!("archived board {slug}"))
        }
        BoardsCommand::Switch { slug } => {
            let slug = boards.resolve(Some(slug), None)?;
            env.scope
                .profile()
                .config_file()
                .set("kanban.current_board", &slug)?;
            print(&format!(
                "profile {} now uses board {slug}",
                env.scope.name()
            ))
        }
        BoardsCommand::Export { slug, file } => {
            let slug = pmt_kanban::boards::slug(slug)?;
            let tasks = transfer::export(boards, &slug, file)?;
            print(&format!(
                "exported board {slug} ({tasks} task(s)) to {}",
                file.display()
            ))
        }
        BoardsCommand::Import { file, rename } => import(env, file, rename.as_deref()),
    }
}

/// `kanban boards list`.
fn list(env: &Env<'_>) -> Result<u8, Error> {
    let list = env.boards.list()?;
    let current = env.slug().ok();
    if env.ctx.json {
        return document(
            "kanban boards list",
            &json!({"boards": list, "current": current}),
        );
    }
    let mut out = String::new();
    for b in &list {
        let mark = if current.as_deref() == Some(b.slug.as_str()) {
            "*"
        } else {
            " "
        };
        let state = if b.settings.archived {
            " (archived)"
        } else {
            ""
        };
        let _ = writeln!(out, "{mark} {}  {}{state}", b.slug, b.settings.name);
    }
    print_raw(&out)
}

/// `kanban boards import`.
fn import(env: &Env<'_>, file: &std::path::Path, rename: Option<&str>) -> Result<u8, Error> {
    let imported = transfer::import(&env.boards, file, rename)?;
    if env.ctx.json {
        return crate::output::json(&imported);
    }
    let mut out = format!(
        "imported board {} ({} task(s))\n",
        imported.board, imported.tasks
    );
    for (old, new) in &imported.remapped {
        let _ = writeln!(out, "  {old} -> {new} (id already used on this home)");
    }
    print_raw(&out)
}

/// `kanban notify ...`.
pub(super) fn notify(env: &Env<'_>, command: &NotifyCommand) -> Result<u8, Error> {
    let mut board = env.board()?;
    match command {
        NotifyCommand::Subscribe {
            id,
            adapter,
            chat,
            thread,
        } => {
            let new = NewSubscription {
                adapter: adapter.clone(),
                chat: chat.clone(),
                thread: thread.clone(),
                profile: env.scope.name().to_string(),
            };
            let sub = board.subscribe(&env.actor(), id, &new, pmt_kanban::now_ms())?;
            print(&format!(
                "subscribed {} ({}:{} as profile {})",
                sub.id, sub.adapter, sub.chat, sub.profile
            ))
        }
        NotifyCommand::List { id } => {
            let subs = board.subscriptions(id.as_deref())?;
            if env.ctx.json {
                return document("kanban notify list", &json!({"subscriptions": subs}));
            }
            let mut out = String::new();
            for s in &subs {
                let thread = s
                    .thread
                    .as_deref()
                    .map(|t| format!(":{t}"))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "{}  {}  {}:{}{thread}  as {}",
                    s.id, s.task_id, s.adapter, s.chat, s.profile
                );
            }
            print_raw(&out)
        }
        NotifyCommand::Unsubscribe { id } => {
            board.unsubscribe(id)?;
            print(&format!("unsubscribed {id}"))
        }
    }
}

/// `kanban gc`.
pub(super) fn gc(env: &Env<'_>) -> Result<u8, Error> {
    let mut board = env.board()?;
    let now = pmt_kanban::now_ms();
    let retention = crate::kanban_host::secs_ms(env.scope.config().kanban.done_sub_retention_secs);
    let subscriptions = board.purge_done_subscriptions(retention, now)?;
    let ticks = board.prune_ticks(now - TICK_RETENTION_MS)?;
    let mut workspaces = 0;
    for task in board.tasks(&Filter {
        archived: true,
        ..Filter::default()
    })? {
        let finished = matches!(task.status, Status::Done | Status::Archived);
        let dir = board.scratch_dir(&task.id);
        if finished
            && task.workspace == Workspace::Scratch
            && dir.exists()
            && std::fs::remove_dir_all(&dir).is_ok()
        {
            workspaces += 1;
        }
    }
    if env.ctx.json {
        return document(
            "kanban gc",
            &json!({"subscriptions": subscriptions, "ticks": ticks, "workspaces": workspaces}),
        );
    }
    print(&format!(
        "gc: {subscriptions} subscription(s), {ticks} tick summary(ies), {workspaces} scratch workspace(s) removed"
    ))
}
