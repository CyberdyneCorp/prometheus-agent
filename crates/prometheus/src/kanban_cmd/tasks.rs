//! Task verbs: create, list, show, edit, link, comment, attach.

use std::fmt::Write as _;
use std::path::PathBuf;

use pmt_core::error::Error;
use pmt_kanban::model::{EvolveMode, ProviderMode};
use pmt_kanban::{Edit, Filter, NewTask, Status, TaskKind, Workspace};
use serde_json::json;

use super::{CreateArgs, Env};
use crate::output::{document, print, print_raw};

fn kind(args: &CreateArgs) -> TaskKind {
    match args.kind.as_str() {
        "eval" => TaskKind::Eval {
            suite: args.suite.clone().unwrap_or_default(),
            provider_mode: match args.provider_mode.as_deref() {
                Some("replay") => Some(ProviderMode::Replay),
                Some(_) => Some(ProviderMode::Live),
                None => None,
            },
        },
        "evolve" => TaskKind::Evolve {
            target_task: args.target_task.clone().unwrap_or_default(),
            mode: if args.mode == "sequential" {
                EvolveMode::Sequential
            } else {
                EvolveMode::Parallel
            },
            budget_k: args.budget_k,
        },
        _ => TaskKind::Agent,
    }
}

/// `kanban create`.
pub(super) fn create(env: &Env<'_>, args: &CreateArgs) -> Result<u8, Error> {
    let mut board = env.board()?;
    let new = NewTask {
        title: args.title.clone(),
        body: args.body.clone().unwrap_or_default(),
        assignee: args.assignee.clone(),
        reviewer: args.reviewer.clone(),
        parents: args.parents.clone(),
        workspace: Workspace::parse(&args.workspace)?,
        kind: kind(args),
        priority: args.priority,
        initial_status: args
            .initial_status
            .as_deref()
            .map(Status::parse)
            .transpose()?,
        idempotency_key: args.idempotency_key.clone(),
        review: if args.review {
            Some(true)
        } else if args.no_review {
            Some(false)
        } else {
            None
        },
        max_retries: args.max_retries,
        budget_usd: args.budget_usd,
        budget_tokens: args.budget_tokens,
        artifacts: args.artifacts.clone(),
    };
    let created = board.create(&env.actor(), &new, &env.profiles(), pmt_kanban::now_ms())?;
    if env.ctx.json {
        return document(
            "kanban create",
            &json!({"board": board.slug(), "id": created.id, "created": created.created, "status": created.status}),
        );
    }
    let verb = if created.created { "created" } else { "exists" };
    print(&format!(
        "{verb} {} ({}) on board {}",
        created.id,
        created.status,
        board.slug()
    ))
}

/// `kanban list`.
pub(super) fn list(
    env: &Env<'_>,
    status: Option<&str>,
    assignee: Option<&str>,
    all: bool,
) -> Result<u8, Error> {
    let board = env.board()?;
    let filter = Filter {
        status: status.map(Status::parse).transpose()?,
        assignee: assignee.map(str::to_owned),
        archived: all,
    };
    let tasks = board.tasks(&filter)?;
    if env.ctx.json {
        return document(
            "kanban list",
            &json!({"board": board.slug(), "tasks": tasks}),
        );
    }
    if tasks.is_empty() {
        return print(&format!("board '{}' is empty", board.slug()));
    }
    let mut out = String::new();
    for t in &tasks {
        let _ = writeln!(
            out,
            "{}  {:<9}  {:<12}  p{}  {}",
            t.id,
            t.status,
            t.assignee.as_deref().unwrap_or("-"),
            t.priority,
            t.title
        );
    }
    print_raw(&out)
}

/// `kanban show`.
pub(super) fn show(env: &Env<'_>, id: &str) -> Result<u8, Error> {
    let board = env.board()?;
    let doc = crate::kanban_host::tool::show(&board, id)?;
    if env.ctx.json {
        return document("kanban show", &doc);
    }
    let task = board.task(id)?;
    let (usd, tokens) = board.spend(id)?;
    let mut out = format!("{} {}\nstatus: {}\n", task.id, task.title, task.status);
    let _ = writeln!(out, "assignee: {}", task.assignee.as_deref().unwrap_or("-"));
    let _ = writeln!(
        out,
        "kind: {}  workspace: {}  priority: {}",
        task.kind.name(),
        task.workspace.spec(),
        task.priority
    );
    if let (Some(kind), Some(reason)) = (task.block_kind, &task.block_reason) {
        let _ = writeln!(out, "blocked: {kind}: {reason}");
    }
    if task.consecutive_failures > 0 {
        let _ = writeln!(
            out,
            "failures: {} (last: {})",
            task.consecutive_failures,
            task.last_failure.as_deref().unwrap_or("-")
        );
    }
    let _ = writeln!(out, "parents: {}", list_or_dash(&board.parents(id)?));
    let _ = writeln!(out, "children: {}", list_or_dash(&board.children(id)?));
    let _ = writeln!(
        out,
        "spend: {usd:.4} USD, {tokens} tokens across {} run(s)",
        board.runs(id)?.len()
    );
    if let Some(result) = task.result.as_deref().or(task.summary.as_deref()) {
        let _ = writeln!(out, "result: {result}");
    }
    if !task.body.is_empty() {
        let _ = writeln!(out, "\n{}", task.body);
    }
    print_raw(&out)
}

fn list_or_dash(ids: &[String]) -> String {
    if ids.is_empty() {
        "-".to_owned()
    } else {
        ids.join(", ")
    }
}

/// `kanban context`.
pub(super) fn context(env: &Env<'_>, id: &str) -> Result<u8, Error> {
    let board = env.board()?;
    print_raw(&pmt_kanban::context::render(&board, id)?)
}

/// `kanban attachments`.
pub(super) fn attachments(env: &Env<'_>, id: &str) -> Result<u8, Error> {
    let board = env.board()?;
    let rows = board.attachments(id)?;
    if env.ctx.json {
        let rows: Vec<_> = rows
            .iter()
            .map(|a| json!({"id": a.id, "filename": a.filename, "content_type": a.content_type, "size": a.size, "uploader": a.uploader, "path": board.attachment_path(a)}))
            .collect();
        return document(
            "kanban attachments",
            &json!({"task_id": id, "attachments": rows}),
        );
    }
    let mut out = String::new();
    for a in &rows {
        let _ = writeln!(
            out,
            "{}  {}  {} bytes  {}  {}",
            a.id,
            a.filename,
            a.size,
            a.uploader,
            board.attachment_path(a).display()
        );
    }
    print_raw(&out)
}

/// `kanban edit`.
pub(super) fn edit(
    env: &Env<'_>,
    id: &str,
    (title, body, priority): (&Option<String>, &Option<String>, &Option<i64>),
    reviewer: Option<&str>,
    review: Option<&str>,
) -> Result<u8, Error> {
    let edit = Edit {
        title: title.clone(),
        body: body.clone(),
        priority: *priority,
        reviewer: reviewer.map(|r| (r != "none").then(|| r.to_owned())),
        review: review.map(|r| match r {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        }),
    };
    if let Some(Some(profile)) = &edit.reviewer
        && !crate::kanban_host::profile_exists(&env.home, profile)
    {
        return Err(Error::new(
            &pmt_core::error::codes::KANBAN_UNKNOWN_PROFILE,
            format!("unknown profile '{profile}'"),
        ));
    }
    env.board()?
        .edit(&env.actor(), id, &edit, pmt_kanban::now_ms())?;
    print(&format!("edited {id}"))
}

/// `kanban link`.
pub(super) fn link(
    env: &Env<'_>,
    parent: &str,
    child: &str,
    run: Option<&str>,
) -> Result<u8, Error> {
    env.board()?
        .link(&env.actor(), parent, child, run, pmt_kanban::now_ms())?;
    print(&format!("{child} now depends on {parent}"))
}

/// `kanban unlink`.
pub(super) fn unlink(env: &Env<'_>, parent: &str, child: &str) -> Result<u8, Error> {
    env.board()?
        .unlink(&env.actor(), parent, child, pmt_kanban::now_ms())?;
    print(&format!("{child} no longer depends on {parent}"))
}

/// `kanban comment`.
pub(super) fn comment(env: &Env<'_>, id: &str, body: &str) -> Result<u8, Error> {
    env.board()?
        .comment(&env.actor(), id, body, pmt_kanban::now_ms())?;
    print(&format!("commented on {id}"))
}

/// `kanban attach`.
pub(super) fn attach(env: &Env<'_>, id: &str, files: &[PathBuf]) -> Result<u8, Error> {
    let mut board = env.board()?;
    let limit = env.scope.config().kanban.attachment_max_bytes;
    let mut out = String::new();
    for file in files {
        let a = board.attach_file(&env.actor(), id, file, limit, pmt_kanban::now_ms())?;
        let _ = writeln!(
            out,
            "attached {} ({} bytes) as {}",
            a.filename, a.size, a.id
        );
    }
    print_raw(&out)
}
