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
pub mod commands;
pub mod display;
pub mod handoff;
pub mod markdown;
pub mod oneshot;
pub mod recap;
pub mod remote;
pub mod repl;
pub mod resume;
pub mod session;
pub(crate) mod signals;
pub mod status_bar;
pub mod terminal;
pub mod theme;
pub mod usage_file;
pub mod worktree;

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
    /// The full-screen terminal UI.
    Tui,
}

/// The mode of this invocation (spec: tui, TUI launch selection; cli,
/// Interface selection is TTY-first): one-shot for `-z` or whenever stdin
/// or stdout is not a terminal (whatever `--tui` or `ui.interface` say);
/// otherwise `--line` or `--tui` when given, the line interface for
/// `-q/--quiet` or a `--usage-file` (the TUI writes neither), else
/// `ui.interface`. No environment variable takes part.
#[must_use]
pub fn mode(args: &ChatArgs, terminals: bool, interface: Interface, quiet: bool) -> Mode {
    if args.oneshot() || !terminals {
        return Mode::OneShot;
    }
    // Safe mode stays in this process on the line interface: the TUI runs
    // over a gateway that reads the profile's configuration.
    if args.line || args.safe_mode {
        return Mode::Line;
    }
    if args.tui {
        return Mode::Tui;
    }
    if quiet || args.usage_file.is_some() || !cfg!(feature = "tui") {
        return Mode::Line;
    }
    match interface {
        Interface::Tui => Mode::Tui,
        Interface::Line => Mode::Line,
    }
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

/// What every safe-mode start shows (spec: plugin-system, Safe mode).
pub const SAFE_MODE_BANNER: &str =
    "safe mode is active: no plugins, MCP servers, sidecars, hooks or webhooks are loaded";

/// An interactive start on a profile with no model offers `setup model`
/// (spec: cli, Setup wizard).
fn offers_setup(args: &ChatArgs, outcome: &Result<u8, Error>) -> bool {
    matches!(outcome, Err(err) if err.code().code == codes::MODEL_NOT_CONFIGURED.code)
        && !args.safe_mode
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
    args.check_safe_mode()?;
    let images = args
        .image
        .iter()
        .map(|path| attach::load(path))
        .collect::<Result<Vec<_>, _>>()?;
    // Before `--cwd` moves: the paths are relative to where the chat ran.
    let image_paths = args
        .image
        .iter()
        .map(|path| std::path::absolute(path).unwrap_or_else(|_| path.clone()))
        .collect();
    if let Some(dir) = &args.cwd {
        change_dir(dir)?;
    }
    let mut set = ctx.set.to_vec();
    set.extend(args.overrides());
    let ctx = Context { set: &set, ..*ctx };
    let scope = if args.ignores_user_config() {
        ctx.scope_defaults()?
    } else {
        ctx.scope()?
    };
    if scope.config().plugins.safe_mode && !ui.quiet {
        eprintln!("{SAFE_MODE_BANNER}");
    }
    let mode = mode(
        args,
        terminal::interactive(),
        scope.config().ui.interface,
        ui.quiet,
    );
    // Only a foreground interactive chat asks about unapproved shell hooks
    // (spec: plugin-system, Shell hooks with first-use consent).
    if mode != Mode::OneShot && terminal::interactive() {
        crate::hooks_host::consent::ask_pending(&scope)?;
    }
    let entered = worktree::enter(args, &scope, ui.quiet)?;
    let run = Run {
        args: entered.as_ref().map_or(args, |e| &e.args),
        ctx: &ctx,
        set: &set,
        scope: &scope,
        mode,
        ui,
        record,
    };
    let outcome = run.start(&source, images, image_paths);
    if let Some(entered) = &entered {
        worktree::finish(entered, mode, &scope, ui.quiet);
    }
    outcome
}

/// One chat run once its profile, mode and directory are settled.
struct Run<'a> {
    args: &'a ChatArgs,
    ctx: &'a Context<'a>,
    set: &'a [String],
    scope: &'a pmt_config::ProfileScope,
    mode: Mode,
    ui: Ui,
    record: &'a Arc<usage_file::RunRecord>,
}

impl Run<'_> {
    /// Reads the first prompt, then runs the session in the TUI, on the
    /// home's daemon or in this process.
    fn start(
        &self,
        source: &PromptSource,
        images: Vec<attach::Image>,
        image_paths: Vec<PathBuf>,
    ) -> Result<u8, Error> {
        let Self {
            args,
            ctx,
            set,
            scope,
            mode,
            ui,
            record,
        } = *self;
        let ctx = *ctx;
        let source = source.clone();
        let scope = scope.clone();
        let set = set.to_vec();
        // The prompt is read before anything starts, so a missing file or an
        // empty pipe fails fast.
        let first = match (mode, &source) {
            (Mode::OneShot, _) => Some(oneshot::prompt(&source)?),
            (Mode::Line | Mode::Tui, PromptSource::Text(text)) => Some(text.clone()),
            (Mode::Line | Mode::Tui, PromptSource::File(path)) => {
                Some(oneshot::read_prompt_file(path)?)
            }
            (Mode::Line | Mode::Tui, PromptSource::None) => None,
        };
        if mode == Mode::Tui {
            let tui = Tui {
                args,
                set: &set,
                first,
                images: image_paths,
            };
            return tui.run(&ctx, &scope, ui);
        }
        if !args.runs_in_process()
            && let Some(daemon) = crate::daemon_client::running(scope.home())?
        {
            let config = scope.config();
            let cwd = current_dir()?;
            let start = remote::Start {
                args,
                mode,
                first,
                cwd: &cwd,
                terminal: terminal::identity().filter(|_| config.sessions.terminal_continue),
                ui,
                approve_unattended: config.approvals.single_query_mode
                    == pmt_config::schema::UnattendedApproval::Approve,
                setup: remote::setup_of(args, &set, &cwd)?,
                images: image_paths,
                run_budget: args.run_budget,
                record: Arc::clone(record),
                scope: &scope,
                launch: ctx.launch,
            };
            return remote::run(&daemon, &scope, &start);
        }
        let _lock = WriterLock::acquire(scope.home(), Role::Chat, vec![scope.name().to_string()])?;
        let ad_hoc = crate::model_cmd::explain::ad_hoc(&args.endpoint)?;
        let options = session_options(args, mode)?;
        let flags = RouteFlags {
            provider: args.provider.as_deref(),
            model: args.model.as_deref(),
            mock_script: args.mock_script.as_deref(),
        };
        let mut factory = Factory::new(scope, ctx.launch, &flags, ad_hoc, options)?
            .with_config_reread(set.clone());
        record.store(&factory.store);
        factory.record = Arc::clone(record);
        let first = First {
            text: first,
            images,
        };
        crate::model_cmd::block_on(start(factory, args, &flags, mode, first, ui))?
    }
}

/// The session flags of a chat in this process.
fn session_options(args: &ChatArgs, mode: Mode) -> Result<Options, Error> {
    Ok(Options {
        interactive: mode == Mode::Line,
        yolo: args.yolo,
        cwd: current_dir()?,
        skills: args.skills.clone(),
        pass_session_id: args.pass_session_id,
        no_context_files: args.no_context_files,
        ignore_user_config: args.ignores_user_config(),
        ignore_rules: args.ignores_rules(),
        source_tag: args.source.clone().or_else(|| {
            (mode == Mode::OneShot).then(|| pmt_store::sessions::ONESHOT_TAG.to_owned())
        }),
        egress: args.egress_overrides(),
        cron: None,
    })
}

/// A chat that opens the TUI.
struct Tui<'a> {
    args: &'a ChatArgs,
    set: &'a [String],
    first: Option<String>,
    images: Vec<PathBuf>,
}

impl Tui<'_> {
    /// Opens the TUI with the chat's session flags; `--in-process` runs
    /// its in-process gateway (`prometheus tui --standalone`).
    #[cfg(feature = "tui")]
    fn run(self, ctx: &Context<'_>, scope: &pmt_config::ProfileScope, ui: Ui) -> Result<u8, Error> {
        if let Some(err) = crate::tui_cmd::not_a_terminal() {
            return Err(err);
        }
        if self.args.usage_file.is_some() {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "--usage-file is not written by the TUI; use --line",
            ));
        }
        let cwd = current_dir()?;
        let launch = crate::tui_cmd::Launch {
            resume: self.args.resume.clone(),
            continue_: self.args.continue_.clone(),
            setup: remote::setup_of(self.args, self.set, &cwd)?,
            standalone: self.args.runs_in_process(),
            first: self.first,
            images: self
                .images
                .iter()
                .map(|p| pmt_rpc::contracts::PromptImage::Path(p.to_string_lossy().into_owned()))
                .collect(),
            run_budget: self.args.run_budget,
            terminal: terminal::identity().filter(|_| scope.config().sessions.terminal_continue),
            cwd,
            egress: self.args.egress_overrides(),
        };
        crate::tui_cmd::start(&launch, scope, ctx, ui)
    }

    /// A build without the TUI answers `--tui` with exit 69.
    #[cfg(not(feature = "tui"))]
    fn run(self, _: &Context<'_>, _: &pmt_config::ProfileScope, _: Ui) -> Result<u8, Error> {
        crate::surface::require("tui").map(|()| 0)
    }
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
        None => factory.create_session(None, &target)?,
    };
    let asks = if mode == Mode::Line {
        let (approver, memory) = repl::memory_channel();
        let (install_asks, installs) = tokio::sync::mpsc::unbounded_channel();
        let (secret_asks, secrets) = tokio::sync::mpsc::unbounded_channel();
        let secret_prompt = Arc::new(repl::CliSecretPrompt::new(secret_asks));
        let (notice_tx, notices) = tokio::sync::mpsc::unbounded_channel();
        let (completion_tx, completions) = tokio::sync::mpsc::unbounded_channel();
        factory = factory
            .with_notices(notice_tx)
            .with_delegation_inbox(Arc::new(session::delegation::LineInbox(completion_tx)))
            .with_memory_approver(approver)
            .with_install_asks(install_asks)
            .with_secret_prompt(Arc::clone(&secret_prompt) as _);
        Some(repl::Asks {
            memory,
            installs,
            secrets,
            secret_prompt: Some(secret_prompt),
            notices,
            completions,
        })
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
        // A TUI run never gets here: it opened the TUI before.
        Mode::Line | Mode::Tui => {
            let asks = asks.unwrap_or_else(repl::Asks::none);
            let start = repl::Start {
                resumed: resumed.is_some(),
                seed: first.text,
                images: first.images,
                run_budget,
                terminal,
            };
            repl::run(factory, live, asks, start, ui).await
        }
    }
}

fn end_reason(status: u8) -> EndReason {
    match status {
        0 => EndReason::UserExit,
        pmt_core::exit::BUDGET_EXHAUSTED => EndReason::BudgetExhausted,
        pmt_core::exit::INTERRUPTED | pmt_core::exit::TERMINATED => EndReason::Interrupted,
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
        Mode::Line | Mode::Tui => println!("{line}"),
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
