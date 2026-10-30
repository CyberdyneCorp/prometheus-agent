//! The `prometheus` binary (alias `pmt`). Every surface is a subcommand of
//! this one executable.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind as ClapErrorKind;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::{exit, panic};
use prometheus::output::print_error;
use prometheus::{
    Cli, Command, Context, backup_cmd, config_cmd, logs_cmd, profile_cmd, secrets_cmd,
    sessions_cmd, startup, trajectory_cmd, version,
};

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return usage_failure(&err),
    };
    let launch = match startup::launch_env() {
        Ok(launch) => launch,
        Err(err) => return failure(&err, cli.json),
    };
    // Commands that need the home report the resolution error themselves.
    let home = pmt_config::home::resolve(cli.home.as_deref(), &launch);
    let home = home.as_ref().ok();
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
    let ctx = Context {
        home_flag: cli.home.as_deref(),
        launch,
        profile: cli.profile.as_deref(),
        set: &cli.set,
        json: cli.json,
    };
    let result = match &cli.command {
        Some(Command::Version) => Ok(version::run(cli.json, cli.verbose > 0)),
        Some(Command::Config { command }) => config_cmd::run(command, &ctx),
        Some(Command::Secrets { command }) => secrets_cmd::run(command, &ctx),
        Some(Command::Profile { command }) => profile_cmd::run(command, &ctx),
        Some(Command::Backup { command }) => backup_cmd::backup(command, &ctx),
        Some(Command::Snapshots { command }) => backup_cmd::snapshots(command, &ctx),
        Some(Command::Sessions { command }) => sessions_cmd::run(command, &ctx),
        Some(Command::Logs(args)) => logs_cmd::run(args, &ctx),
        Some(Command::Trajectory { command }) => trajectory_cmd::run(command, &ctx),
        // `chat`, the default command, arrives with the CLI chat group.
        None => Ok(exit::SUCCESS),
    };
    result.unwrap_or_else(|err| {
        print_error(&err, cli.json);
        err.exit_code()
    })
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
    if display_only || !json_requested() {
        err.exit();
    }
    let rendered = err.to_string();
    let first_line = rendered.lines().next().unwrap_or_default();
    let message = first_line.strip_prefix("error: ").unwrap_or(first_line);
    let usage = Error::new(&codes::CLI_USAGE, message).with_hint("run: prometheus help");
    failure(&usage, true)
}

/// Whether `--json` appears before any `--` separator. Used only when the
/// command line did not parse, to pick the error format.
fn json_requested() -> bool {
    std::env::args_os()
        .skip(1)
        .take_while(|arg| arg != OsStr::new("--"))
        .any(|arg| arg == OsStr::new("--json"))
}
