//! `prometheus kanban` (spec: kanban, CLI surface; Additional CLI verbs;
//! Board slugs and selection; Board export and import; /kanban slash
//! command; projects, Kanban board binding). Every verb works on the
//! board named by `--board`, else the board bound to the project owning
//! the current directory (or the active project), else the invoking
//! profile's `kanban.current_board`, else `default`; lifecycle verbs take
//! several ids, report each, and exit 1 when any failed. The same verbs
//! run as `/kanban` on every chat surface, their output captured instead
//! of printed.

mod args;
mod boards;
mod dispatch;
mod history;
mod lifecycle;
mod more;
mod orchestrate;
mod tasks;
pub mod ticketed;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_kanban::{Actor, Board, Boards};
use serde::Serialize;

pub use self::args::{BoardsCommand, CreateArgs, KanbanArgs, KanbanCommand, NotifyCommand};

use crate::context::Context;
use crate::kanban_host::hooks::HookQueue;

/// What every subcommand works with.
pub(crate) struct Env<'a> {
    pub ctx: &'a Context<'a>,
    pub scope: ProfileScope,
    pub home: PathBuf,
    pub boards: Boards,
    pub flag: Option<&'a str>,
    /// The directory the command runs in (project board selection).
    pub cwd: PathBuf,
    /// `/kanban`: output collected here instead of printed.
    pub capture: Option<RefCell<String>>,
    /// Lifecycle hooks the command's board work queued.
    pub hooks: Arc<HookQueue>,
    /// `/kanban` from a messaging chat: the chat a created task reports
    /// back to.
    pub origin: Option<pmt_kanban::notify::NewSubscription>,
}

impl Env<'_> {
    /// Prints `value` and a newline (or captures it).
    pub(crate) fn print(&self, value: &impl std::fmt::Display) -> Result<u8, Error> {
        match &self.capture {
            Some(out) => {
                use std::fmt::Write as _;
                let _ = writeln!(out.borrow_mut(), "{value}");
                Ok(0)
            }
            None => crate::output::print(value),
        }
    }

    /// Prints `text` as is (or captures it).
    pub(crate) fn raw(&self, text: &str) -> Result<u8, Error> {
        match &self.capture {
            Some(out) => {
                out.borrow_mut().push_str(text);
                Ok(0)
            }
            None => crate::output::print_raw(text),
        }
    }

    /// Prints the JSON document of `command` (or captures it).
    pub(crate) fn document(&self, command: &str, body: &impl Serialize) -> Result<u8, Error> {
        match &self.capture {
            Some(out) => {
                let mut bytes = Vec::new();
                crate::output::document_to(&mut bytes, command, body)?;
                out.borrow_mut().push_str(&String::from_utf8_lossy(&bytes));
                Ok(0)
            }
            None => crate::output::document(command, body),
        }
    }

    /// The selected board's slug.
    pub(crate) fn slug(&self) -> Result<String, Error> {
        let flag = self
            .flag
            .map(str::to_owned)
            .or_else(|| crate::kanban_host::project_board(&self.scope, &self.cwd));
        self.boards.resolve(
            flag.as_deref(),
            self.scope.config().kanban.current_board.as_deref(),
        )
    }

    /// The selected board.
    pub(crate) fn board(&self) -> Result<Board, Error> {
        self.boards.board(&self.slug()?)
    }

    /// A person at the CLI, named by the invoking profile.
    pub(crate) fn actor(&self) -> Actor {
        Actor::user(self.scope.name().as_str())
    }

    /// Who tasks created or assigned here may name.
    pub(crate) fn profiles(&self) -> crate::kanban_host::assignees::Assignees {
        crate::kanban_host::assignees::Assignees::of(&self.scope)
    }
}

/// Runs one `kanban` subcommand.
///
/// # Errors
///
/// Unknown boards and tasks (66), invalid input (65, 2), illegal
/// transitions and a held dispatcher lease (75), store errors.
pub fn run(args: &KanbanArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    if let KanbanCommand::Work { run } = &args.command {
        return crate::kanban_host::worker::work(ctx, run);
    }
    if args.ticket.is_some() {
        return ticketed::run(args, ctx.home()?.root(), ctx.json);
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let env = open(ctx, args.board.as_deref(), cwd, None)?;
    let result = dispatch_verb(&env, args);
    env.hooks.flush(&env.scope);
    result
}

/// The environment of one `kanban` verb run by `ctx` in `cwd` (`capture`:
/// collect output for `/kanban`).
pub(crate) fn open<'a>(
    ctx: &'a Context<'a>,
    flag: Option<&'a str>,
    cwd: PathBuf,
    capture: Option<RefCell<String>>,
) -> Result<Env<'a>, Error> {
    let scope = ctx.scope()?;
    let home = scope.home().to_path_buf();
    let hooks = HookQueue::new(&scope);
    let boards = crate::kanban_host::boards(&scope, &hooks)?;
    Ok(Env {
        ctx,
        scope,
        home,
        boards,
        flag,
        cwd,
        capture,
        hooks,
        origin: None,
    })
}

/// Runs the verb of `args` in `env`.
pub(crate) fn dispatch_verb(env: &Env<'_>, args: &KanbanArgs) -> Result<u8, Error> {
    match &args.command {
        KanbanCommand::Create(create) => tasks::create(env, create),
        KanbanCommand::List {
            status,
            assignee,
            all,
        } => tasks::list(env, status.as_deref(), assignee.as_deref(), *all),
        KanbanCommand::Show { id } => tasks::show(env, id),
        KanbanCommand::Context { id } => tasks::context(env, id),
        KanbanCommand::Attachments { id } => tasks::attachments(env, id),
        KanbanCommand::Log { id } => history::log(env, id),
        KanbanCommand::Runs { id } => history::runs(env, id),
        KanbanCommand::Tail { lines, follow } => history::tail(env, *lines, *follow),
        KanbanCommand::Stats => history::stats(env),
        KanbanCommand::Diag => history::diag(env),
        KanbanCommand::Dispatch {
            once: _,
            looping,
            dry_run,
        } => {
            if *dry_run {
                dispatch::dry_run(env)
            } else {
                dispatch::dispatch(env, *looping)
            }
        }
        KanbanCommand::Notify { command } => boards::notify(env, command),
        KanbanCommand::Gc => boards::gc(env),
        KanbanCommand::Boards { command } => boards::boards(env, command),
        other => more::run(env, other).unwrap_or_else(|| edit(env, other)),
    }
}

/// The verbs that change tasks.
fn edit(env: &Env<'_>, command: &KanbanCommand) -> Result<u8, Error> {
    match command {
        KanbanCommand::Edit {
            id,
            title,
            body,
            priority,
            reviewer,
            review,
        } => tasks::edit(
            env,
            id,
            (title, body, priority),
            reviewer.as_deref(),
            review.as_deref(),
        ),
        KanbanCommand::Link { parent, child, run } => {
            tasks::link(env, parent, child, run.as_deref())
        }
        KanbanCommand::Unlink { parent, child } => tasks::unlink(env, parent, child),
        KanbanCommand::Comment { id, body } => tasks::comment(env, id, body),
        KanbanCommand::Attach { id, files } => tasks::attach(env, id, files),
        other => lifecycle::run(env, other),
    }
}
