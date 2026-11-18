//! `prometheus computer-use screen status|start|stop [--force]|install
//! [-y]` (spec: execution-environments, Bot screen; Bot screen takeover
//! lease): the profile's bot screen.

use std::fmt::Write as _;

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_config::screen_lease::{self, ScreenLease};
use pmt_core::error::Error;
use serde_json::{Value, json};

use crate::context::Context;
use crate::output::{document, print_raw};
use crate::screen::{self, install};

/// `prometheus computer-use` subcommands.
#[derive(Debug, Subcommand)]
pub enum ComputerUseCommand {
    /// The bot screen: the virtual desktop the agent's headed browsers act
    /// on.
    Screen {
        /// The screen subcommand.
        #[command(subcommand)]
        command: ScreenCommand,
    },
}

/// `prometheus computer-use screen` subcommands.
#[derive(Debug, Subcommand)]
pub enum ScreenCommand {
    /// Whether the screen runs, its display, size, idle time and who holds
    /// it.
    Status,
    /// Start the screen.
    Start,
    /// Stop the screen.
    Stop {
        /// Also take it back from a person holding it.
        #[arg(long)]
        force: bool,
    },
    /// Install Xvnc and a minimal Xfce session with the package manager
    /// (shows the packages first).
    Install {
        /// Do not ask before installing.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Run the screen in this process (started by `start`).
    #[command(hide = true)]
    Serve,
}

/// Runs one `computer-use` subcommand.
///
/// # Errors
///
/// `screen.unavailable`, `screen.human_has_control`, `home.io`.
pub fn run(command: &ComputerUseCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let ComputerUseCommand::Screen { command } = command;
    if let ScreenCommand::Install { yes } = command {
        return install_packages(*yes);
    }
    let scope = ctx.scope()?;
    match command {
        ScreenCommand::Status => status(&scope, ctx.json),
        ScreenCommand::Start => {
            let record = screen::start(&scope)?;
            eprintln!(
                "bot screen running on display {} ({})",
                record.display, record.geometry
            );
            Ok(0)
        }
        ScreenCommand::Stop { force } => {
            let stopped = screen::stop(&scope, *force)?;
            eprintln!(
                "{}",
                if stopped {
                    "bot screen stopped"
                } else {
                    "the bot screen was not running"
                }
            );
            Ok(0)
        }
        ScreenCommand::Serve => serve(&scope),
        ScreenCommand::Install { .. } => unreachable!("handled above"),
    }
}

#[cfg(unix)]
fn serve(scope: &ProfileScope) -> Result<u8, Error> {
    screen::serve::serve(scope)
}

#[cfg(not(unix))]
fn serve(_scope: &ProfileScope) -> Result<u8, Error> {
    Err(Error::new(
        &pmt_core::error::codes::SCREEN_UNAVAILABLE,
        "the bot screen needs a Unix host",
    ))
}

fn status(scope: &ProfileScope, json: bool) -> Result<u8, Error> {
    let lease = ScreenLease::of(scope.dir()).read(screen_lease::now());
    let body = match screen::running(scope) {
        Some((_, status)) => json!({"running": true, "screen": status}),
        None => json!({"running": false}),
    };
    let mut body = body;
    body["holder"] = lease.holder.map_or(Value::Null, |h| json!(h.viewer));
    body["epoch"] = json!(lease.epoch);
    if json {
        return document("computer-use screen status", &body);
    }
    print_raw(&render(&body))
}

fn render(body: &Value) -> String {
    let mut out = String::new();
    if body["running"] == true {
        let screen = &body["screen"];
        let place = match screen["placement"].as_str() {
            Some("container") => " in the desktop container",
            Some("ssh") => " on the SSH host",
            _ => "",
        };
        let _ = writeln!(
            out,
            "bot screen: running{place} on display {} ({}), idle {} s",
            screen["backend_display"]
                .as_str()
                .or_else(|| screen["display"].as_str())
                .unwrap_or_default(),
            screen["geometry"].as_str().unwrap_or_default(),
            screen["idle_secs"].as_u64().unwrap_or_default()
        );
    } else {
        out.push_str("bot screen: not running\n");
    }
    match body["holder"].as_str() {
        Some(holder) => {
            let _ = writeln!(out, "control: {holder} (the agent waits)");
        }
        None => out.push_str("control: agent\n"),
    }
    out
}

fn install_packages(yes: bool) -> Result<u8, Error> {
    let path = std::env::var_os("PATH");
    let Some((plan, manager)) = install::plan(path.as_deref()) else {
        return Err(Error::new(
            &pmt_core::error::codes::SCREEN_UNAVAILABLE,
            "no supported package manager (apt-get, dnf, pacman, zypper, apk) was found",
        )
        .with_hint("install Xvnc (TigerVNC) and xfce4-session yourself"));
    };
    let how = install::how(is_root(), install::find("sudo", path.as_deref()));
    let sudo = matches!(how, install::Run::Sudo(_));
    let line = install::command_line(&plan, &manager, sudo);
    eprintln!("packages: {}", plan.packages.join(" "));
    if how == install::Run::PrintOnly {
        println!("{}", line.join(" "));
        eprintln!("run this command as root to install them");
        return Ok(0);
    }
    if !yes && !crate::prompt::confirm(&format!("run `{}`?", line.join(" ")))? {
        eprintln!("not installed");
        return Ok(1);
    }
    let program = match &how {
        install::Run::Sudo(sudo) => sudo.clone(),
        _ => manager.clone(),
    };
    let status = std::process::Command::new(program)
        .args(&line[1..])
        .status()
        .map_err(|e| {
            Error::new(
                &pmt_core::error::codes::SCREEN_UNAVAILABLE,
                format!("cannot run {}: {e}", manager.display()),
            )
        })?;
    if status.success() {
        eprintln!("installed");
        Ok(0)
    } else {
        Err(Error::new(
            &pmt_core::error::codes::SCREEN_UNAVAILABLE,
            format!("{} failed ({status})", manager.display()),
        ))
    }
}

#[cfg(unix)]
fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

#[cfg(not(unix))]
fn is_root() -> bool {
    false
}
