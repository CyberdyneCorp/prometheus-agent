//! `prometheus chat`, the default command (spec: cli, Chat flags; One-shot
//! and piped mode; Interface selection is TTY-first; Resume and continue
//! resolution; Seed prompt and prompt file).
//!
//! The flow: validate the prompt flags, change to `--cwd`, open the
//! profile with the flags' overrides, take the home's writer lock, pick
//! the session (new, `-r`, `-c`), build it ([`session::Factory`]) and run
//! it one-shot ([`oneshot`]) or in the line interface ([`repl`]).

pub mod args;
pub mod attach;
pub mod oneshot;
pub mod recap;
pub mod repl;
pub mod resume;
pub mod session;
mod signals;
pub mod terminal;
pub mod theme;
pub mod usage_file;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::TurnInput;
use pmt_config::schema::Interface;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_core::runtime::{Role, WriterLock};
use pmt_store::EndReason;

use self::args::{ChatArgs, PromptSource};
use self::session::model::RouteFlags;
use self::session::{Factory, Options};
use crate::context::Context;

/// The global flags that shape output.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)] // independent global switches
pub struct Ui {
    /// `--json`.
    pub json: bool,
    /// `-q/--quiet`.
    pub quiet: bool,
    /// `--no-color`.
    pub no_color: bool,
    /// `-v` count.
    pub verbose: u8,
}

/// How the session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// One turn, the answer on stdout.
    OneShot,
    /// The line interface.
    Line,
}

/// The mode of this invocation: one-shot for `-z` or whenever stdin or
/// stdout is not a terminal (whatever `--tui` or `ui.interface` say);
/// otherwise the line interface, which also stands in for the TUI until
/// that surface lands.
#[must_use]
pub fn mode(args: &ChatArgs, terminals: bool, interface: Interface) -> Mode {
    if args.oneshot() || !terminals {
        return Mode::OneShot;
    }
    // `--line`, `--tui` and `ui.interface = "tui"` all run the line
    // interface in this build.
    let _ = interface;
    Mode::Line
}

/// Runs `chat`.
///
/// # Errors
///
/// Usage, configuration, store and session errors (the caller prints and
/// exits with their code); a finished run returns its own status.
pub fn run(args: &ChatArgs, ctx: &Context<'_>, ui: Ui) -> Result<u8, Error> {
    let record = Arc::new(usage_file::RunRecord::default());
    let mut outcome = run_recorded(args, ctx, ui, &record);
    if offers_setup(args, &outcome) && crate::setup_cmd::offer(ctx)? {
        outcome = run_recorded(args, ctx, ui, &record);
    }
    match &args.usage_file {
        Some(path) => usage_file::finish(path, &record, outcome, ui.json),
        None => outcome,
    }
}

/// An interactive start on a profile with no model offers `setup model`
/// (spec: cli, Setup wizard).
fn offers_setup(args: &ChatArgs, outcome: &Result<u8, Error>) -> bool {
    matches!(outcome, Err(err) if err.code().code == codes::MODEL_NOT_CONFIGURED.code)
        && !args.oneshot()
        && terminal::interactive()
}

fn run_recorded(
    args: &ChatArgs,
    ctx: &Context<'_>,
    ui: Ui,
    record: &Arc<usage_file::RunRecord>,
) -> Result<u8, Error> {
    let source = args.prompt_source()?;
    let images = args
        .image
        .iter()
        .map(|path| attach::load(path))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(dir) = &args.cwd {
        change_dir(dir)?;
    }
    let mut set = ctx.set.to_vec();
    set.extend(args.overrides());
    let ctx = Context { set: &set, ..*ctx };
    let scope = if args.ignore_user_config {
        ctx.scope_defaults()?
    } else {
        ctx.scope()?
    };
    let mode = mode(args, terminal::interactive(), scope.config().ui.interface);
    // The prompt is read before anything starts, so a missing file or an
    // empty pipe fails fast.
    let first = match (mode, &source) {
        (Mode::OneShot, _) => Some(oneshot::prompt(&source)?),
        (Mode::Line, PromptSource::Text(text)) => Some(text.clone()),
        (Mode::Line, PromptSource::File(path)) => Some(oneshot::read_prompt_file(path)?),
        (Mode::Line, PromptSource::None) => None,
    };
    let _lock = WriterLock::acquire(scope.home(), Role::Chat, vec![scope.name().to_string()])?;
    let ad_hoc = crate::model_cmd::explain::ad_hoc(&args.endpoint)?;
    let options = Options {
        interactive: mode == Mode::Line,
        yolo: args.yolo,
        cwd: current_dir()?,
        skills: args.skills.clone(),
        pass_session_id: args.pass_session_id,
        no_context_files: args.no_context_files,
        ignore_user_config: args.ignore_user_config,
        ignore_rules: args.ignore_rules,
        source_tag: args.source.clone().or_else(|| {
            (mode == Mode::OneShot).then(|| pmt_store::sessions::ONESHOT_TAG.to_owned())
        }),
    };
    let flags = RouteFlags {
        provider: args.provider.as_deref(),
        model: args.model.as_deref(),
        mock_script: args.mock_script.as_deref(),
    };
    let mut factory = Factory::new(scope, ctx.launch, &flags, ad_hoc, options)?;
    record.store(&factory.store);
    factory.record = Arc::clone(record);
    let first = First {
        text: first,
        images,
    };
    crate::model_cmd::block_on(start(factory, args, &flags, mode, first, ui))?
}

/// The first prompt and the `--image` attachments that go with it.
struct First {
    text: Option<String>,
    images: Vec<attach::Image>,
}

async fn start(
    mut factory: Factory,
    args: &ChatArgs,
    flags: &RouteFlags<'_>,
    mode: Mode,
    first: First,
    ui: Ui,
) -> Result<u8, Error> {
    let target = factory.models.primary(flags).await?;
    let terminal =
        terminal::identity().filter(|_| factory.scope.config().sessions.terminal_continue);
    let request = resume::Request::of(args.resume.as_deref(), args.continue_.as_deref());
    let resumed = resume::resolve(
        &factory.store,
        &request,
        resume::Place {
            cwd: &factory.options.cwd,
            terminal: terminal.as_deref(),
        },
    )?;
    if let Some(id) = resumed {
        factory
            .store
            .check_resumable(id, factory.scope.config().sessions.max_resume_messages)?;
        factory.store.reopen_session(id)?;
        if args.cwd.is_none() {
            restore_cwd(&mut factory, id, mode)?;
        }
    }
    let id = match resumed {
        Some(id) => id,
        None => factory.create_session(None)?,
    };
    let memory = if mode == Mode::Line {
        let (approver, asks) = repl::memory_channel();
        factory = factory.with_memory_approver(approver);
        Some(asks)
    } else {
        None
    };
    let live = factory.open(id, &target).await?;
    if let Some(terminal) = &terminal {
        factory.store.record_terminal(id, terminal)?;
    }
    let run_budget = args.run_budget.map(Duration::from_secs);
    match mode {
        Mode::OneShot => {
            let images = match attach::prepare(&factory, &live, &first.images).await {
                Ok(images) => images,
                Err(err) => {
                    live.close(&factory.store, EndReason::Error).await;
                    return Err(err);
                }
            };
            let input = TurnInput {
                images,
                ..TurnInput::text(first.text.unwrap_or_default())
            };
            let status = oneshot::run(&live, input, ui.json, run_budget).await;
            live.close(&factory.store, end_reason(status)).await;
            Ok(status)
        }
        Mode::Line => {
            let memory = memory.unwrap_or_else(|| repl::memory_channel().1);
            let start = repl::Start {
                resumed: resumed.is_some(),
                seed: first.text,
                images: first.images,
                run_budget,
                terminal,
            };
            repl::run(factory, live, memory, start, ui).await
        }
    }
}

fn end_reason(status: u8) -> EndReason {
    match status {
        0 => EndReason::UserExit,
        oneshot::BUDGET_EXHAUSTED => EndReason::BudgetExhausted,
        oneshot::INTERRUPTED | oneshot::TERMINATED => EndReason::Interrupted,
        _ => EndReason::Error,
    }
}

/// Moves to the resumed session's recorded directory when it still
/// exists (`cwd restored to <dir>`).
fn restore_cwd(factory: &mut Factory, id: SessionId, mode: Mode) -> Result<(), Error> {
    let session = factory.store.session(id)?;
    let Some(recorded) = session.cwd.map(PathBuf::from) else {
        return Ok(());
    };
    if !recorded.is_dir() || same_dir(&recorded, &factory.options.cwd) {
        return Ok(());
    }
    change_dir(&recorded)?;
    factory.options.cwd = current_dir()?;
    let line = format!("cwd restored to {}", recorded.display());
    match mode {
        Mode::Line => println!("{line}"),
        Mode::OneShot => eprintln!("{line}"),
    }
    Ok(())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn change_dir(dir: &Path) -> Result<(), Error> {
    std::env::set_current_dir(dir).map_err(|e| {
        Error::new(
            &codes::CHAT_CWD_MISSING,
            format!("cannot use {} as the working directory: {e}", dir.display()),
        )
    })
}

fn current_dir() -> Result<PathBuf, Error> {
    std::env::current_dir().map_err(|e| {
        Error::new(
            &codes::CHAT_CWD_MISSING,
            format!("no working directory: {e}"),
        )
    })
}
