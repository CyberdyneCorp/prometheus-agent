//! `prometheus codex-runtime` and `/codex-runtime` (spec: model-providers,
//! Codex app-server runtime; Codex runtime migration): which runtime runs
//! turns on the OpenAI providers, changed from the next session only.
//! Turning the app-server on (and `migrate`) needs the Codex executable,
//! refusing with an install hint and the configuration unchanged without
//! it, and writes the managed section of the Codex config first.

pub mod migrate;

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::PathBuf;

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_config::schema::OpenaiRuntime;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};

use crate::context::Context;
use crate::output::{json, print};

/// `prometheus codex-runtime` subcommands.
#[derive(Debug, Subcommand)]
pub enum CodexRuntimeCommand {
    /// The configured runtime and the Codex executable it would run.
    Status,
    /// Set `model.openai_runtime` for new sessions: `auto` (or `off`) or
    /// `codex_app_server` (or `on`).
    Set {
        /// `auto`, `codex_app_server`, `on` or `off`.
        mode: String,
    },
    /// Write the profile's MCP servers, the enabled Codex plugins and the
    /// Prometheus callback server into the managed section of the Codex
    /// config, and use the app-server from the next session.
    Migrate {
        /// Show what would be written; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Runs one `codex-runtime` subcommand.
///
/// # Errors
///
/// `provider.external_cli_missing` without the Codex executable,
/// `cli.usage` for an unknown mode, config and I/O errors.
pub fn run(command: &CodexRuntimeCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    match command {
        CodexRuntimeCommand::Status => {
            let text = status(&scope, ctx.launch);
            emit(ctx.json, &serde_json::json!({"text": text}), &text)
        }
        CodexRuntimeCommand::Set { mode } => {
            let text = switch(&scope, ctx.launch, mode)?;
            emit(ctx.json, &serde_json::json!({"text": text}), &text)
        }
        CodexRuntimeCommand::Migrate { dry_run } => {
            let program = executable(&scope, ctx.launch)?;
            let report = migrate(&scope, ctx.launch, &program, *dry_run)?;
            if !dry_run {
                set(&scope, OpenaiRuntime::CodexAppServer)?;
            }
            emit(ctx.json, &report, &describe(&report))
        }
    }
}

fn emit(as_json: bool, value: &impl serde::Serialize, text: &str) -> Result<u8, Error> {
    if as_json { json(value) } else { print(&text) }
}

/// `/codex-runtime [auto|codex_app_server|on|off]`: the status, or a
/// switch that applies from the next session.
///
/// # Errors
///
/// As [`run`].
pub fn slash(scope: &ProfileScope, launch: &LaunchEnv, args: &str) -> Result<String, Error> {
    match args.trim() {
        "" | "status" => Ok(status(scope, launch)),
        mode => switch(scope, launch, mode),
    }
}

fn parse(mode: &str) -> Result<OpenaiRuntime, Error> {
    match mode {
        "auto" | "off" => Ok(OpenaiRuntime::Auto),
        "codex_app_server" | "on" => Ok(OpenaiRuntime::CodexAppServer),
        other => Err(Error::new(
            &codes::CLI_USAGE,
            format!("unknown runtime '{other}'; use auto, codex_app_server, on or off"),
        )),
    }
}

/// Sets the runtime for new sessions; turning the app-server on checks
/// the executable and migrates first.
fn switch(scope: &ProfileScope, launch: &LaunchEnv, mode: &str) -> Result<String, Error> {
    let runtime = parse(mode)?;
    let mut text = String::new();
    if runtime == OpenaiRuntime::CodexAppServer {
        let program = executable(scope, launch)?;
        let report = migrate(scope, launch, &program, false)?;
        text = describe(&report);
        text.push('\n');
    }
    set(scope, runtime)?;
    let _ = write!(
        text,
        "model.openai_runtime = {}\nnew sessions use it; this session keeps its runtime",
        runtime.as_str()
    );
    Ok(text)
}

fn set(scope: &ProfileScope, runtime: OpenaiRuntime) -> Result<(), Error> {
    scope
        .profile()
        .config_file()
        .set("model.openai_runtime", runtime.as_str())
}

fn status(scope: &ProfileScope, launch: &LaunchEnv) -> String {
    let runtime = scope.config().model.openai_runtime;
    let codex = match executable(scope, launch) {
        Ok(path) => path.display().to_string(),
        Err(err) => format!("not found ({})", err.hint().unwrap_or(err.message())),
    };
    format!(
        "model.openai_runtime = {} (changes apply from the next session)\ncodex: {codex}",
        runtime.as_str()
    )
}

/// The Codex executable new sessions would run.
fn executable(scope: &ProfileScope, launch: &LaunchEnv) -> Result<PathBuf, Error> {
    pmt_providers::codex::executable(
        scope.config().model.codex_bin.as_deref(),
        launch.inherited("PATH").map(OsString::from),
        launch.user_home().map(PathBuf::as_path),
    )
}

/// Runs the migration against the Codex config (`dry_run`: nothing is
/// written).
fn migrate(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    program: &std::path::Path,
    dry_run: bool,
) -> Result<migrate::Report, Error> {
    let codex_home = pmt_providers::codex::codex_home(
        launch.inherited("CODEX_HOME").map(OsString::from),
        launch.user_home().map(PathBuf::as_path),
    )
    .ok_or_else(|| Error::new(&codes::HOME_IO, "no home directory for the Codex config"))?;
    let target = codex_home.join("config.toml");
    let existing = match std::fs::read_to_string(&target) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(io_error(&target, &err.to_string())),
    };
    let mut errors = Vec::new();
    let plugins = match listed_plugins(program) {
        Ok(plugins) => plugins,
        Err(err) => {
            errors.push(format!("codex plugin/list: {err}"));
            Vec::new()
        }
    };
    let inputs = migrate::Inputs {
        servers: &scope.config().mcp.servers,
        plugins: &plugins,
        callback: callback(scope),
    };
    let (text, mut report) =
        migrate::plan(&existing, &inputs, &target).map_err(|e| io_error(&target, &e))?;
    report.errors.splice(0..0, errors);
    if !dry_run {
        migrate::write(&target, &text).map_err(|e| io_error(&target, &e))?;
        report.written = true;
    }
    Ok(report)
}

fn io_error(path: &std::path::Path, why: &str) -> Error {
    Error::new(
        &codes::HOME_IO,
        format!("cannot migrate {}: {why}", path.display()),
    )
}

/// The installed and available plugins Codex reports. It runs on a thread
/// of its own: `/codex-runtime` calls it from inside a session's runtime.
fn listed_plugins(program: &std::path::Path) -> Result<Vec<String>, String> {
    let list = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?
            .block_on(pmt_providers::codex::plugins(program))
    };
    let plugins = std::thread::scope(|scope| scope.spawn(list).join())
        .map_err(|_| "plugin/list panicked".to_owned())??;
    Ok(plugins
        .into_iter()
        .filter(|p| p.installed && p.available)
        .map(|p| p.name)
        .collect())
}

/// `prometheus --profile <p> mcp serve --stdio --tools ...`.
fn callback(scope: &ProfileScope) -> (String, Vec<String>) {
    let program = std::env::current_exe()
        .map_or_else(|_| "prometheus".to_owned(), |p| p.display().to_string());
    let args = [
        "--profile",
        scope.name().as_str(),
        "mcp",
        "serve",
        "--stdio",
        "--tools",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .chain(std::iter::once(migrate::CALLBACK_TOOLS.join(",")))
    .collect();
    (program, args)
}

fn describe(report: &migrate::Report) -> String {
    let verb = if report.written {
        "migrated"
    } else {
        "would migrate"
    };
    let mut text = format!(
        "{verb} {} MCP server(s) into {} ({} of yours kept)",
        report.migrated.len(),
        report.target_path,
        report.preserved_user_servers.len()
    );
    for (server, keys) in &report.skipped_keys_per_server {
        let _ = write!(
            text,
            "\nwarning: {server}: dropped {} (no Codex equivalent)",
            keys.join(", ")
        );
    }
    for error in &report.errors {
        let _ = write!(text, "\nerror: {error}");
    }
    text
}
