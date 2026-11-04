//! The `prometheus` binary (alias `pmt`). Every surface is a subcommand of
//! this one executable.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::error::ErrorKind as ClapErrorKind;
use clap::{CommandFactory, Parser};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::panic;
use prometheus::output::print_error;
use prometheus::{
    Cli, Command, Context, approvals_cmd, auth_cmd, backup_cmd, chat, config_cmd, demo_cmd,
    fallback_cmd, grants_cmd, logs_cmd, memory_cmd, model_cmd, profile_cmd, prompt_cmd,
    runtimes_cmd, secrets_cmd, self_test_cmd, sessions_cmd, skills_cmd, startup, tools_cmd,
    trajectory_cmd, update_cmd, usage_cmd, version,
};

fn main() -> ExitCode {
    // A copy of this binary started as the Windows sandbox launcher only
    // starts the confined child; it parses no CLI and reads no environment.
    if let Some(code) = pmt_sandbox::launcher::run_if_requested() {
        std::process::exit(code);
    }
    // UTF-8 console (and child consoles) on Windows until main returns.
    let _console = pmt_sandbox::console::Utf8Console::enable();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return usage_failure(&err),
    };
    if cli.print_version {
        return ExitCode::from(version::run(cli.json, cli.verbose > 0));
    }
    if cli.command.is_some() && !cli.chat.is_empty() {
        let usage = Error::new(
            &codes::CLI_USAGE,
            "chat flags go without a subcommand or after `chat`",
        )
        .with_hint("run: prometheus chat --help");
        return failure(&usage, cli.json);
    }
    let launch = match startup::launch_env() {
        Ok(launch) => launch,
        Err(err) => return failure(&err, cli.json),
    };
    // Completion runs on every TAB: it reads local files only, before
    // anything (logging, the home layout) could write one.
    if let Some(Command::Complete {
        shell,
        current,
        words,
    }) = &cli.command
    {
        let done = prometheus::completions::complete(*shell, current.as_deref(), words, &launch);
        return ExitCode::from(done.unwrap_or(0));
    }
    // Commands that need the home report the resolution error themselves.
    // `demo` runs in a temporary home of its own and never touches the
    // user's: it writes no log files and keeps no crash reports there.
    // The administrator's managed layer is machine-wide: loaded once, for
    // every profile this process touches.
    let (managed_dir, owner) = match launch.test_managed_dir() {
        Some(dir) => (dir.clone(), pmt_config::managed::Owner::CurrentUser),
        None => (
            pmt_config::managed::platform_dir(),
            pmt_config::managed::Owner::Root,
        ),
    };
    pmt_config::managed::init(&managed_dir, owner);
    // A quick command never shadows a command of the shared registry
    // (spec: messaging-gateway, Gateway slash commands, quick commands and
    // hooks): the names are reserved before any config loads.
    pmt_config::gateway_check::reserve_command_names(prometheus::slash::reserved_names());
    pmt_config::gateway_check::register_toolsets(prometheus::tools_cmd::expand_toolsets);
    pmt_config::gateway_check::register_skills(prometheus::skills_cmd::policy_skill_resolver(
        launch.user_home().cloned(),
    ));
    pmt_config::gateway_check::register_roles(prometheus::daemon_client::role_resolver());
    let home = pmt_config::home::resolve(cli.home.as_deref(), &launch)
        .ok()
        .filter(|_| !matches!(cli.command, Some(Command::Demo(_))));
    let home = home.as_ref();
    startup::install_logging(home, &launch);
    // `profile` is recorded once a command selects one (startup::bind_profile).
    let command_span = tracing::info_span!("command", profile = tracing::field::Empty);
    let _command = command_span.enter();
    startup::warn_ignored_env(&launch);
    startup::raise_nofile(home.map(PathBuf::as_path));
    let reporter = startup::crash_reporter(home, &launch);
    match panic::catch_main(&reporter, || run(&cli, &launch)) {
        Ok(code) => ExitCode::from(code),
        Err(err) => failure(&err, cli.json),
    }
}

fn run(cli: &Cli, launch: &LaunchEnv) -> u8 {
    let mut set = cli.set.clone();
    if cli.accept_hooks {
        set.push("hooks.auto_accept=true".to_owned());
    }
    if cli.safe_mode {
        set.push("plugins.safe_mode=true".to_owned());
    }
    let ctx = Context {
        home_flag: cli.home.as_deref(),
        launch,
        profile: cli.profile.as_deref(),
        set: &set,
        json: cli.json,
    };
    let result = match &cli.command {
        Some(Command::Version) => Ok(version::run(cli.json, cli.verbose > 0)),
        Some(Command::Setup(args)) => prometheus::setup_cmd::run(args, &ctx),
        Some(Command::Doctor(args)) => prometheus::doctor_cmd::run(args, &ctx),
        Some(Command::Debug { command }) => prometheus::debug_cmd::run(command, &ctx),
        Some(Command::Config { command }) => config_cmd::run(command, &ctx),
        Some(Command::Secrets { command }) => secrets_cmd::run(command, &ctx),
        Some(Command::Grants { command }) => grants_cmd::run(command, &ctx),
        Some(Command::Skills { command }) => skills_cmd::run(command, &ctx),
        Some(Command::Memory { command }) => memory_cmd::run(command, &ctx),
        Some(Command::Profile { command }) => profile_cmd::run(command, &ctx),
        Some(Command::Backup { command }) => backup_cmd::backup(command, &ctx),
        Some(Command::Snapshots { command }) => backup_cmd::snapshots(command, &ctx),
        Some(Command::Sessions { command }) => sessions_cmd::run(command, &ctx, ui(cli)),
        Some(Command::Logs(args)) => logs_cmd::run(args, &ctx),
        Some(Command::Trajectory { command }) => trajectory_cmd::run(command, &ctx),
        Some(Command::Demo(args)) => demo_cmd::run(args, launch),
        Some(Command::Prompt { command }) => prompt_cmd::run(command, &ctx),
        Some(Command::Usage(args)) => usage_cmd::run(args, &ctx),
        Some(Command::Model { command }) => model_cmd::run(command.as_ref(), &ctx),
        Some(Command::Fallback { command }) => fallback_cmd::run(command, &ctx),
        Some(Command::Auth { command }) => auth_cmd::run(command, &ctx),
        Some(Command::Tools { command }) => tools_cmd::run(command, &ctx),
        Some(Command::Toolsets { command }) => tools_cmd::toolsets(command, &ctx),
        Some(Command::Approvals { command }) => approvals_cmd::run(command, &ctx),
        Some(Command::Update(args)) => update_cmd::run(args, &ctx),
        Some(Command::Pause(args)) => prometheus::pause_cmd::run(args, &ctx),
        #[cfg(feature = "gateway")]
        Some(Command::Daemon { command }) => prometheus::daemon_cmd::run(command, &ctx),
        #[cfg(feature = "gateway")]
        Some(Command::Gateway { command }) => prometheus::daemon_cmd::gateway(command, &ctx),
        #[cfg(feature = "gateway")]
        Some(Command::Pairing { command }) => prometheus::daemon_cmd::pairing(command, &ctx),
        #[cfg(not(feature = "gateway"))]
        Some(Command::Daemon(_)) => prometheus::surface::require("gateway").map(|()| 0),
        Some(Command::Plugins { command }) => prometheus::plugins_cmd::run(command, &ctx),
        #[cfg(feature = "mcp")]
        Some(Command::Mcp { command }) => prometheus::mcp_cmd::run(command.as_ref(), &ctx),
        Some(Command::Rpc { command }) => prometheus::rpc_cmd::run(command, &ctx),
        Some(Command::Connectors { command }) => prometheus::connectors_cmd::run(command, &ctx),
        Some(Command::SelfTest) => Ok(self_test_cmd::run(&ctx)),
        Some(Command::Completions { shell }) => prometheus::completions::script(*shell),
        Some(Command::Complete { .. }) => Ok(0),
        Some(Command::Runtimes { command }) => runtimes_cmd::run(command, &ctx),
        Some(Command::Chat(args)) => chat::run(&safe(args, cli), &ctx, ui(cli)),
        #[cfg(feature = "tui")]
        Some(Command::Tui(args)) => prometheus::tui_cmd::run(args, &ctx, ui(cli)),
        #[cfg(not(feature = "tui"))]
        Some(Command::Tui(_)) => prometheus::surface::require("tui").map(|()| 0),
        #[cfg(not(feature = "gateway"))]
        Some(Command::Gateway(_)) => prometheus::surface::require("gateway").map(|()| 0),
        #[cfg(not(feature = "dashboard"))]
        Some(Command::Dashboard(_)) => prometheus::surface::require("dashboard").map(|()| 0),
        #[cfg(not(feature = "acp"))]
        Some(Command::Acp(_)) => prometheus::surface::require("acp").map(|()| 0),
        #[cfg(not(feature = "mcp"))]
        Some(Command::Mcp(_)) => prometheus::surface::require("mcp").map(|()| 0),
        #[cfg(not(feature = "evolve"))]
        Some(Command::Evolve(_)) => prometheus::surface::require("evolve").map(|()| 0),
        #[cfg(not(feature = "eval"))]
        Some(Command::Eval(_)) => prometheus::surface::require("eval").map(|()| 0),
        None => chat::run(&safe(&cli.chat, cli), &ctx, ui(cli)),
    };
    result.unwrap_or_else(|err| {
        print_error(&err, cli.json);
        err.exit_code()
    })
}

/// The chat flags with the global `--safe-mode`.
fn safe(args: &prometheus::cli::ChatArgs, cli: &Cli) -> prometheus::cli::ChatArgs {
    prometheus::cli::ChatArgs {
        safe_mode: cli.safe_mode,
        ..args.clone()
    }
}

fn ui(cli: &Cli) -> chat::Ui {
    chat::Ui {
        json: cli.json,
        quiet: cli.quiet,
        no_color: cli.no_color,
        verbose: cli.verbose,
    }
}

fn failure(err: &Error, json: bool) -> ExitCode {
    print_error(err, json);
    ExitCode::from(err.exit_code())
}

/// Help and `--version` print normally. A usage error is clap's text, or
/// with `--json` the `cli.usage` error document; both exit 2.
fn usage_failure(err: &clap::Error) -> ExitCode {
    let display_only = matches!(
        err.kind(),
        ClapErrorKind::DisplayHelp
            | ClapErrorKind::DisplayVersion
            | ClapErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    if let Some(unknown) = unknown_command(err) {
        return failure(&unknown, json_requested());
    }
    if err.kind() == ClapErrorKind::DisplayHelp && top_level_help() {
        let _ = err.print();
        if let Some(section) = plugin_commands_help() {
            print!("{section}");
        }
        return ExitCode::SUCCESS;
    }
    if display_only || !json_requested() {
        err.exit();
    }
    let rendered = err.to_string();
    let first_line = rendered.lines().next().unwrap_or_default();
    let message = first_line.strip_prefix("error: ").unwrap_or(first_line);
    let usage = Error::new(&codes::CLI_USAGE, message).with_hint("run: prometheus help");
    failure(&usage, true)
}

/// Whether the command line asks for the top-level help (`prometheus
/// help`, `prometheus --help`).
fn top_level_help() -> bool {
    let (_, words) = help_words();
    matches!(words.as_slice(), [one] if one == "help" || one == "-h" || one == "--help")
}

/// The `--home` value and the other words of the command line.
fn help_words() -> (Option<PathBuf>, Vec<String>) {
    let mut home = None;
    let mut words = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--home" {
            home = args.next().map(PathBuf::from);
        } else if let Some(value) = arg.strip_prefix("--home=") {
            home = Some(PathBuf::from(value));
        } else if !arg.starts_with('-') || arg == "-h" || arg == "--help" {
            words.push(arg);
        }
    }
    (home, words)
}

/// The enabled plugins' commands of the selected profile (spec:
/// plugin-system, Plugin CLI subcommands).
fn plugin_commands_help() -> Option<String> {
    let launch = startup::launch_env().ok()?;
    let (home, _) = help_words();
    let ctx = prometheus::context::Context {
        home_flag: home.as_deref(),
        launch: &launch,
        profile: None,
        set: &[],
        json: false,
    };
    let scope = ctx.scope().ok()?;
    prometheus::plugins_cmd::run_cmd::help_section(&scope)
}

/// `unknown command 'sesions' (did you mean 'sessions'?)` for a mistyped
/// subcommand (spec: cli, Unknown subcommand).
fn unknown_command(err: &clap::Error) -> Option<Error> {
    use clap::error::{ContextKind, ContextValue};
    if err.kind() != ClapErrorKind::InvalidSubcommand {
        return None;
    }
    let Some(ContextValue::String(name)) = err.get(ContextKind::InvalidSubcommand) else {
        return None;
    };
    let command = Cli::command();
    let names = command
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .map(clap::Command::get_name);
    let message = match pmt_core::registry::closest(name, names).as_slice() {
        [] => format!("unknown command '{name}'"),
        found => format!(
            "unknown command '{name}' (did you mean {}?)",
            pmt_core::registry::or_list(found)
        ),
    };
    Some(Error::new(&codes::CLI_USAGE, message).with_hint("run: prometheus help"))
}

/// Whether `--json` appears before any `--` separator. Used only when the
/// command line did not parse, to pick the error format.
fn json_requested() -> bool {
    std::env::args_os()
        .skip(1)
        .take_while(|arg| arg != OsStr::new("--"))
        .any(|arg| arg == OsStr::new("--json"))
}
