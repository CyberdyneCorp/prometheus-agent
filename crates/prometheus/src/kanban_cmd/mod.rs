//! `prometheus kanban` (spec: kanban, CLI surface; Board slugs and
//! selection; Board export and import). Every verb works on the board
//! named by `--board`, else the invoking profile's `kanban.current_board`,
//! else `default`; lifecycle verbs take several ids, report each, and
//! exit 1 when any failed.

mod args;
mod boards;
mod dispatch;
mod history;
mod lifecycle;
mod tasks;

use std::path::PathBuf;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_kanban::{Actor, Board, Boards};

pub use self::args::{BoardsCommand, CreateArgs, KanbanArgs, KanbanCommand, NotifyCommand};

use crate::context::Context;

/// What every subcommand works with.
pub(crate) struct Env<'a> {
    pub ctx: &'a Context<'a>,
    pub scope: ProfileScope,
    pub home: PathBuf,
    pub boards: Boards,
    pub flag: Option<&'a str>,
}

impl Env<'_> {
    /// The selected board's slug.
    pub(crate) fn slug(&self) -> Result<String, Error> {
        self.boards.resolve(
            self.flag,
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

    pub(crate) fn profiles(&self) -> crate::kanban_host::HomeProfiles {
        crate::kanban_host::HomeProfiles(self.home.clone())
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
    let scope = ctx.scope()?;
    let home = scope.home().to_path_buf();
    let boards = Boards::open(&home)?;
    let env = Env {
        ctx,
        scope,
        home,
        boards,
        flag: args.board.as_deref(),
    };
    match &args.command {
        KanbanCommand::Create(create) => tasks::create(&env, create),
        KanbanCommand::List {
            status,
            assignee,
            all,
        } => tasks::list(&env, status.as_deref(), assignee.as_deref(), *all),
        KanbanCommand::Show { id } => tasks::show(&env, id),
        KanbanCommand::Context { id } => tasks::context(&env, id),
        KanbanCommand::Attachments { id } => tasks::attachments(&env, id),
        KanbanCommand::Log { id } => history::log(&env, id),
        KanbanCommand::Runs { id } => history::runs(&env, id),
        KanbanCommand::Tail { lines, follow } => history::tail(&env, *lines, *follow),
        KanbanCommand::Stats => history::stats(&env),
        KanbanCommand::Diag => history::diag(&env),
        KanbanCommand::Dispatch { once: _, looping } => dispatch::dispatch(&env, *looping),
        KanbanCommand::Notify { command } => boards::notify(&env, command),
        KanbanCommand::Gc => boards::gc(&env),
        KanbanCommand::Boards { command } => boards::boards(&env, command),
        other => edit(&env, other),
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
