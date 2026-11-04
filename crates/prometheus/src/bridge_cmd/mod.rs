//! `prometheus bridge` (spec: python-bridge, Bridge diagnostics; Named side
//! environments): the profile's python-bridge sidecars, their logs, a
//! restart that clears a failed plugin, a doctor for the runtime, the
//! environments and the sandbox, and named side environments for skill
//! scripts.

mod doctor;
mod named;

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;
use pmt_core::error::{Error, codes};
use pmt_pybridge::log::Log;
use pmt_pybridge::supervisor;

use crate::context::Context;
use crate::output::{document, print_raw};

/// `prometheus bridge` subcommands.
#[derive(Debug, Subcommand)]
pub enum BridgeCommand {
    /// Every sidecar of the profile: plugin, profile, pid, state
    /// (`starting|running|idle|crashed|failed`), uptime, restarts, memory.
    Status,
    /// A plugin's bridge log (`<profile>/logs/bridge/<id>.log`).
    Logs {
        /// The plugin id.
        id: String,
        /// Keep printing what is appended.
        #[arg(long)]
        follow: bool,
    },
    /// Restart a plugin's sidecar, clearing a `failed` state.
    Restart {
        /// The plugin id.
        id: String,
    },
    /// Check the managed runtime, every environment against its lock and
    /// the sandbox enforcement level (exit 1 when a check fails).
    Doctor,
    /// Materialize a named pinned environment and print only its
    /// interpreter's path on stdout.
    Env {
        /// `^[a-z0-9][a-z0-9_-]{0,63}$`.
        name: String,
        /// A requirements file (repeatable).
        #[arg(long = "requirement", short = 'r', value_name = "FILE")]
        requirements: Vec<PathBuf>,
        /// One requirement (repeatable), e.g. `pypdf>=4,<5`.
        #[arg(long = "extra", value_name = "SPEC")]
        extras: Vec<String>,
    },
}

/// Runs `prometheus bridge`.
///
/// # Errors
///
/// Profile errors, the command's own.
pub fn run(command: &BridgeCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        BridgeCommand::Status => status(ctx),
        BridgeCommand::Logs { id, follow } => logs(ctx, id, *follow),
        BridgeCommand::Restart { id } => restart(ctx, id),
        BridgeCommand::Doctor => doctor::run(ctx),
        BridgeCommand::Env {
            name,
            requirements,
            extras,
        } => named::run(ctx, name, requirements, extras),
    }
}

fn status(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let rows = supervisor::live(scope.dir());
    if ctx.json {
        return document("bridge status", &serde_json::json!({"sidecars": rows}));
    }
    if rows.is_empty() {
        return print_raw("no bridge sidecars are running for this profile\n");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut text = String::from("PLUGIN  PROFILE  PID  STATE  UPTIME  RESTARTS  MEMORY\n");
    for row in rows {
        let pid = row.pid.map_or_else(|| "-".to_owned(), |p| p.to_string());
        let uptime = row.started_at.map_or_else(
            || "-".to_owned(),
            |at| format!("{}s", now.saturating_sub(at)),
        );
        let _ = writeln!(
            text,
            "{}  {}  {pid}  {}  {uptime}  {}  {} MiB",
            row.plugin,
            row.profile,
            row.state.as_str(),
            row.restarts,
            row.memory / (1024 * 1024)
        );
    }
    print_raw(&text)
}

fn logs(ctx: &Context<'_>, id: &str, follow: bool) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let path = Log::path_of(scope.dir(), id);
    let mut offset = 0u64;
    loop {
        let bytes = std::fs::read(&path).unwrap_or_default();
        if offset == 0 && bytes.is_empty() && !follow {
            return Err(Error::new(
                &codes::PLUGINS_NOT_FOUND,
                format!("no bridge log for plugin '{id}'"),
            )
            .with_hint(format!("expected {}", path.display())));
        }
        let len = bytes.len() as u64;
        if len < offset {
            // Rotated: start over.
            offset = 0;
        }
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        print_raw(&String::from_utf8_lossy(&bytes[start..]))?;
        offset = len;
        if !follow {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn restart(ctx: &Context<'_>, id: &str) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    // A supervisor in this process restarts at once; one in the daemon
    // picks the request up at its next use.
    if let Some(supervisor) = crate::plugins_host::bridge::running(&scope)
        .into_iter()
        .find(|s| s.plugin() == id)
    {
        supervisor.restart()?;
        return print_raw(&format!("restarted plugin '{id}'\n"));
    }
    let request = supervisor::restart_request(scope.dir(), id);
    if let Some(dir) = request.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(&codes::BRIDGE_CALL, dir, &e))?;
    }
    std::fs::write(&request, b"").map_err(|e| Error::io(&codes::BRIDGE_CALL, &request, &e))?;
    print_raw(&format!(
        "restart requested for plugin '{id}': its failed state clears at its next use\n"
    ))
}
