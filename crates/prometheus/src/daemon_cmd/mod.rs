//! `prometheus daemon` (spec: messaging-gateway, One gateway daemon per
//! home; workspace-architecture, One daemon per Prometheus home; Daemon
//! discovery file and local control channel; Stale daemon build
//! detection): `run` serves in the foreground, `start` starts it detached
//! (doing nothing when one runs), `ensure --json` starts it when absent and
//! prints its endpoint, `stop` drains and stops it, `restart` drains and
//! replaces it, `status` prints its process state.

mod enroll_cmd;
mod gateway_cmd;
mod host;
mod inventory;
mod messaging;
mod send_cmd;
mod serve;
pub mod service;
pub(crate) mod standalone;
#[cfg(debug_assertions)]
mod test_adapter;
mod turn;

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use clap::Subcommand;
use pmt_core::error::{Error, codes};
use pmt_rpc::control::{self, ControlRequest, DaemonStatus};
use pmt_rpc::discovery::{DaemonRecord, Discovery, discover};
use serde::Serialize;

use crate::context::Context;

/// `prometheus daemon` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground (what `start` launches).
    Run,
    /// Start the daemon in the background; does nothing when it runs.
    Start,
    /// Start the daemon when it is not running and print its endpoint
    /// (with `--json`: one object for clients that spawn it).
    Ensure,
    /// Stop the daemon after its running turns finish.
    Stop,
    /// Replace the daemon (after a binary update): drain running turns,
    /// stop it and start this binary's daemon.
    Restart,
    /// Print the daemon's process state.
    Status,
    /// Install a per-user supervisor unit that starts the daemon at login
    /// (no administrator rights); `--system --run-as-user` installs a
    /// boot-time unit instead (root only).
    InstallService(service::InstallArgs),
    /// Stop and remove the unit `install-service` installed.
    UninstallService(service::UninstallArgs),
}

pub use gateway_cmd::{GatewayCommand, OutboxCommand, PairingCommand};

/// Runs `prometheus gateway <command>`.
///
/// # Errors
///
/// As [`gateway_cmd::run`].
pub fn gateway(command: &GatewayCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    gateway_cmd::run(command, ctx)
}

/// Runs `prometheus pairing <command>`.
///
/// # Errors
///
/// As [`gateway_cmd::pairing`].
pub fn pairing(command: &PairingCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    gateway_cmd::pairing(command, ctx)
}

/// How long `start` waits for the endpoint.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// Runs `prometheus daemon <command>`.
///
/// # Errors
///
/// Home, lock, configuration and control-channel errors.
pub fn run(command: &DaemonCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?;
    let root = home.root().to_path_buf();
    match command {
        DaemonCommand::Run => serve::run(&root, ctx),
        DaemonCommand::Start => start(&root, ctx),
        DaemonCommand::Ensure => ensure(&root, ctx),
        DaemonCommand::Stop => stop(&root, ctx.json),
        DaemonCommand::Restart => restart(&root, ctx),
        DaemonCommand::Status => status(&root, ctx.json),
        DaemonCommand::InstallService(args) => service::install(args, ctx),
        DaemonCommand::UninstallService(args) => service::uninstall(args, ctx),
    }
}

/// What `ensure --json` prints.
#[derive(Debug, Serialize)]
struct EnsureOutput<'a> {
    rpc_url: &'a str,
    control: &'a str,
    instance_id: &'a str,
    build_id: &'a str,
    rpc_protocol: &'a str,
}

fn start(home: &Path, ctx: &Context<'_>) -> Result<u8, Error> {
    let (record, started) = running_or_start(home, ctx.profile)?;
    if ctx.json {
        return ensure_output(&record);
    }
    let pid = record.process.pid;
    if started {
        crate::output::print(&format!(
            "daemon started (pid {pid}, {})",
            record.endpoint.rpc_url
        ))
    } else {
        crate::output::print(&format!("gateway already running (pid {pid})"))
    }
}

fn ensure(home: &Path, ctx: &Context<'_>) -> Result<u8, Error> {
    let (record, _) = running_or_start(home, ctx.profile)?;
    if ctx.json {
        return ensure_output(&record);
    }
    let e = &record.endpoint;
    crate::output::print(&format!(
        "rpc_url: {}\ncontrol: {}\ninstance_id: {}\nbuild_id: {}\nrpc_protocol: {}",
        e.rpc_url, e.control, e.instance_id, e.build_id, e.rpc_protocol
    ))
}

fn ensure_output(record: &DaemonRecord) -> Result<u8, Error> {
    let e = &record.endpoint;
    crate::output::json(&EnsureOutput {
        rpc_url: &e.rpc_url,
        control: &e.control,
        instance_id: &e.instance_id,
        build_id: &e.build_id,
        rpc_protocol: &e.rpc_protocol,
    })
}

/// The running daemon, starting one when there is none; `true` when this
/// call started it.
///
/// # Errors
///
/// `daemon.start_failed`, `home.io`.
pub(crate) fn running_or_start(
    home: &Path,
    profile: Option<&str>,
) -> Result<(DaemonRecord, bool), Error> {
    if let Some(record) = wait_running(home, Duration::from_secs(5))? {
        return Ok((record, false));
    }
    let mut child = spawn_detached(home, profile)?;
    let deadline = std::time::Instant::now() + START_TIMEOUT;
    loop {
        if let Discovery::Running(record) = discover(home)? {
            return Ok((record, true));
        }
        if let Ok(Some(status)) = child.try_wait() {
            // Another start won the race: its daemon is the one to use.
            if let Some(record) = wait_running(home, Duration::from_secs(5))? {
                return Ok((record, false));
            }
            let why = match last_stderr_line(home) {
                Some(line) => format!("the daemon exited with {status}: {line}"),
                None => format!("the daemon exited with {status}"),
            };
            return Err(start_failed(&why));
        }
        if std::time::Instant::now() > deadline {
            return Err(start_failed(
                "the daemon did not publish its endpoint in time",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn start_failed(why: &str) -> Error {
    Error::new(
        &codes::DAEMON_START_FAILED,
        format!("cannot start the daemon: {why}"),
    )
    .with_hint("run 'prometheus daemon run' to see the error, or 'prometheus logs'")
}

/// The running daemon; a daemon still starting gets `wait` to publish.
fn wait_running(home: &Path, wait: Duration) -> Result<Option<DaemonRecord>, Error> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match discover(home)? {
            Discovery::Running(record) => return Ok(Some(record)),
            Discovery::NotRunning => return Ok(None),
            Discovery::Starting(_) if std::time::Instant::now() > deadline => return Ok(None),
            Discovery::Starting(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// Where a detached daemon's stderr goes: what it prints before its log
/// file is set up (a startup error) stays readable.
fn stderr_path(home: &Path) -> std::path::PathBuf {
    home.join("logs").join("daemon-stderr.log")
}

fn stderr_file(home: &Path) -> std::io::Result<std::fs::File> {
    let path = stderr_path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::File::create(path)
}

/// The last non-empty line the daemon printed to stderr.
fn last_stderr_line(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(stderr_path(home)).ok()?;
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty() && !line.starts_with("hint:"))
        .map(|line| line.trim_start_matches("error: ").to_owned())
}

/// `<this binary> --home <home> [-p <profile>] daemon run`, detached from
/// this process group and terminal.
fn spawn_detached(home: &Path, profile: Option<&str>) -> Result<std::process::Child, Error> {
    let exe = std::env::current_exe()
        .map_err(|e| start_failed(&format!("cannot find this binary: {e}")))?;
    let stderr = stderr_file(home).map_err(|e| {
        start_failed(&format!(
            "cannot create {}: {e}",
            stderr_path(home).display()
        ))
    })?;
    let mut command = Command::new(exe);
    command.arg("--home").arg(home);
    if let Some(profile) = profile {
        command.args(["--profile", profile]);
    }
    command
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(windows)]
    {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        std::os::windows::process::CommandExt::creation_flags(
            &mut command,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
        );
    }
    command
        .spawn()
        .map_err(|e| start_failed(&format!("cannot launch it: {e}")))
}

fn control_call(
    record: &DaemonRecord,
    request: &ControlRequest,
) -> Result<control::ControlResponse, Error> {
    let control = record.endpoint.control.clone();
    let request = request.clone();
    crate::model_cmd::block_on(async move { control::call(&control, &request).await })?
}

/// Asks the daemon to stop and waits until it has; `false` when none ran.
fn stop_and_wait(home: &Path, restart: bool, wait: Duration) -> Result<bool, Error> {
    let Some(record) = wait_running(home, Duration::from_secs(5))? else {
        return Ok(false);
    };
    control_call(&record, &ControlRequest::Stop { restart })?;
    let deadline = std::time::Instant::now() + wait;
    while !matches!(discover(home)?, Discovery::NotRunning) {
        if std::time::Instant::now() > deadline {
            return Err(Error::new(
                &codes::DAEMON_START_FAILED,
                format!(
                    "the daemon (pid {}) did not stop in time",
                    record.process.pid
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(true)
}

/// How long a drain may take, plus slack for the shutdown itself.
fn drain_wait(home: &Path) -> Duration {
    let drain = pmt_config::DaemonConfig::load(home).map_or(120, |c| c.daemon.restart_drain_secs);
    Duration::from_secs(drain + 15)
}

fn stop(home: &Path, json: bool) -> Result<u8, Error> {
    let stopped = stop_and_wait(home, false, drain_wait(home))?;
    if json {
        return crate::output::json(&serde_json::json!({"stopped": stopped}));
    }
    crate::output::print(&if stopped {
        "daemon stopped"
    } else {
        "gateway is not running"
    })
}

fn restart(home: &Path, ctx: &Context<'_>) -> Result<u8, Error> {
    stop_and_wait(home, true, drain_wait(home))?;
    start(home, ctx)
}

/// `daemon status --json`.
#[derive(Debug, Serialize)]
struct StatusOutput {
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon: Option<DaemonStatus>,
}

fn status(home: &Path, json: bool) -> Result<u8, Error> {
    let daemon = match wait_running(home, Duration::from_secs(2))? {
        Some(record) => control_call(&record, &ControlRequest::Status)?.status,
        None => None,
    };
    if json {
        let running = daemon.is_some();
        return crate::output::document("daemon status", &StatusOutput { running, daemon });
    }
    let Some(d) = daemon else {
        return crate::output::print(&"gateway is not running");
    };
    let mut text = format!(
        "gateway running (pid {}, up {} s)\nversion: {} (build {})\nrpc: {} (protocol {})\nprofiles: {}\nconnections: {}, sessions: {}, running turns: {}",
        d.pid,
        d.uptime_secs,
        d.version,
        d.build_id,
        d.rpc_url,
        d.rpc_protocol,
        d.profiles.join(", "),
        d.connections,
        d.sessions,
        d.active_turns
    );
    if d.build_id != crate::build_info::BUILD_ID {
        let _ = write!(
            text,
            "\ndaemon is running an older build ({}); restart with 'prometheus daemon restart'",
            d.version
        );
    }
    if d.retiring {
        text.push_str("\nretiring: draining running turns before it stops");
    }
    for profile in &d.config {
        if let Some(error) = &profile.error {
            let _ = write!(
                text,
                "\nconfig error ({}): {error}; serving the previous configuration",
                profile.profile
            );
        }
        if !profile.pending_restart.is_empty() {
            let _ = write!(
                text,
                "\nrestart pending ({}): {} change(s) apply after 'prometheus daemon restart': {}",
                profile.profile,
                profile.pending_restart.len(),
                profile.pending_restart.join(", ")
            );
        }
    }
    crate::output::print(&text)
}
