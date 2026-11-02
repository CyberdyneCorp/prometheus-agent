//! `prometheus tui` and the TUI side of `prometheus`/`prometheus chat` on
//! a terminal (spec: tui, TUI launch selection; TUI attaches to the gateway
//! daemon as a client; Standalone in-process mode; Terminal state is
//! always restored; In-TUI update hands off without a relaunch protocol).
//!
//! The TUI owns only the screen: the session, its agent and its approvals
//! live in the home's gateway daemon, which is started detached when it
//! is not running (`daemon.autostart`, default true). `--standalone` runs
//! an in-process gateway instead that holds the home for the TUI's
//! lifetime and serves only it. The screen ([`pmt_tui`]) is fed from the
//! session's events by [`screen`]; [`link`] is the protocol side.

mod input;
mod link;
mod masked;
mod screen;
mod session;
mod signals;

use std::io::IsTerminal as _;
use std::path::PathBuf;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{PromptImage, SessionSetup};
use pmt_rpc::discovery::DaemonRecord;

use crate::chat::Ui;
use crate::context::Context;

/// `prometheus tui` flags.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct TuiArgs {
    /// Run an in-process gateway that holds this home and serves only this
    /// TUI, instead of attaching to the home's daemon.
    #[arg(long)]
    pub standalone: bool,
    /// Resume a session: an id, an id prefix, a title or `latest`.
    #[arg(short = 'r', long, value_name = "REF", conflicts_with = "continue_")]
    pub resume: Option<String>,
    /// Continue the newest session of this directory, or of the lineage
    /// titled NAME.
    #[arg(short = 'c', long = "continue", value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    pub continue_: Option<String>,
    /// The model of a new session (`[<provider>/]<model>` or an alias).
    #[arg(short = 'm', long, value_name = "MODEL")]
    pub model: Option<String>,
    /// The provider of a new session.
    #[arg(long, value_name = "ID")]
    pub provider: Option<String>,
}

/// What a TUI run opens.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    /// `-r <ref>`.
    pub resume: Option<String>,
    /// `-c [name]`.
    pub continue_: Option<String>,
    /// How the daemon runs the session.
    pub setup: SessionSetup,
    /// An in-process gateway instead of the daemon.
    pub standalone: bool,
    /// A prompt submitted once the session is open (`--query`).
    pub first: Option<String>,
    /// Images that go with it.
    pub images: Vec<PromptImage>,
    /// `--run-budget` seconds, per turn.
    pub run_budget: Option<u64>,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// This terminal's identity (`-c` prefers its last session).
    pub terminal: Option<String>,
}

/// How the TUI ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ending {
    /// `/quit` or the quit key: exit 0.
    Quit,
    /// A signal: exit with its status.
    Signal(u8),
    /// `/update [flags]`: run the updater once the terminal is restored.
    Update(Vec<String>),
}

/// The message for `prometheus tui` without a terminal.
pub(crate) fn not_a_terminal() -> Option<Error> {
    let which = if !std::io::stdin().is_terminal() {
        "stdin"
    } else if !std::io::stdout().is_terminal() {
        "stdout"
    } else {
        return None;
    };
    Some(Error::new(
        &codes::TUI_NOT_A_TERMINAL,
        format!("prometheus tui needs an interactive terminal ({which} is not a TTY)"),
    ))
}

/// Runs `prometheus tui`.
///
/// # Errors
///
/// `tui.not_a_terminal` (2), configuration errors (78), `daemon.not_running`
/// (69), `home.locked` (75), connection and session errors.
pub fn run(args: &TuiArgs, ctx: &Context<'_>, ui: Ui) -> Result<u8, Error> {
    if let Some(err) = not_a_terminal() {
        return Err(err);
    }
    let scope = ctx.scope()?;
    let cwd = std::env::current_dir().map_err(|e| {
        Error::new(
            &codes::CHAT_CWD_MISSING,
            format!("no working directory: {e}"),
        )
    })?;
    let launch = Launch {
        resume: args.resume.clone(),
        continue_: args.continue_.clone(),
        setup: SessionSetup {
            model: args.model.clone(),
            provider: args.provider.clone(),
            set: ctx.set.to_vec(),
            ..SessionSetup::default()
        },
        standalone: args.standalone,
        cwd,
        terminal: crate::chat::terminal::identity()
            .filter(|_| scope.config().sessions.terminal_continue),
        ..Launch::default()
    };
    start(&launch, &scope, ctx, ui)
}

/// Opens the TUI for `launch` (also the path of `prometheus` and
/// `prometheus chat` on a terminal with `ui.interface = "tui"`).
///
/// # Errors
///
/// As [`run`].
pub fn start(
    launch: &Launch,
    scope: &ProfileScope,
    ctx: &Context<'_>,
    ui: Ui,
) -> Result<u8, Error> {
    let bindings = pmt_tui::keys::Bindings::new(&scope.config().ui.tui.keys)
        .map_err(|message| Error::new(&codes::CONFIG_INVALID, message))?;
    let home = scope.home().to_path_buf();
    let daemon = if launch.standalone {
        None
    } else {
        Some(daemon(&home, ctx.profile)?)
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot start the runtime: {e}")))?;
    let open = screen::Open {
        launch,
        scope,
        bindings,
        no_color: ui.no_color
            || ctx
                .launch
                .inherited("NO_COLOR")
                .is_some_and(|v| !v.is_empty()),
        home: home.clone(),
        panic_on_draw: ctx.launch.test_tui_panic(),
    };
    let ending = runtime.block_on(async {
        match daemon {
            Some(record) => screen::run(&open, record).await,
            None => standalone(&open, ctx).await,
        }
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    match ending? {
        Ending::Quit => Ok(0),
        Ending::Signal(code) => Ok(code),
        Ending::Update(args) => update(&args, ctx),
    }
}

/// The home's running daemon, started detached when it is not running
/// and `daemon.autostart` allows it.
fn daemon(home: &std::path::Path, profile: Option<&str>) -> Result<DaemonRecord, Error> {
    if let Some(record) = crate::daemon_client::running(home)? {
        return Ok(record);
    }
    if !pmt_config::DaemonConfig::load(home)?.daemon.autostart {
        return Err(Error::new(
            &codes::DAEMON_NOT_RUNNING,
            "gateway daemon is not running; start it with 'prometheus daemon start'",
        ));
    }
    crate::daemon_cmd::running_or_start(home, profile).map(|(record, _)| record)
}

async fn standalone(open: &screen::Open<'_>, ctx: &Context<'_>) -> Result<Ending, Error> {
    use crate::daemon_cmd::standalone::{Setup, Standalone, lock};
    let name = open.scope.name().as_str().to_owned();
    let lock = lock(&open.home, vec![name])?;
    let setup = Setup {
        home: open.home.clone(),
        scope: open.scope.clone(),
        launch: ctx.launch.clone(),
        set: ctx.set.to_vec(),
    };
    let (gateway, record) = Standalone::start(lock, setup).await?;
    let ending = screen::run(open, record).await;
    gateway.stop().await;
    ending
}

/// `/update`: the same code path as `prometheus update`, run once the
/// terminal is restored; its exit status is the process's.
fn update(args: &[String], ctx: &Context<'_>) -> Result<u8, Error> {
    use clap::Parser as _;
    #[derive(clap::Parser)]
    struct Update {
        #[command(flatten)]
        args: crate::cli::UpdateArgs,
    }
    let words = std::iter::once("update".to_owned()).chain(args.iter().cloned());
    let parsed = Update::try_parse_from(words).map_err(|e| {
        let text = e.to_string();
        let first = text.lines().next().unwrap_or_default();
        Error::new(
            &codes::CLI_USAGE,
            first.trim_start_matches("error: ").to_owned(),
        )
    })?;
    crate::update_cmd::run(&parsed.args, ctx)
}
