//! `prometheus pause` (spec: messaging-gateway, Global emergency pause):
//! stops new work on every profile of the home (no new turns from any
//! surface, no scheduled job fires) until `prometheus pause off`; turns
//! already running finish and slash commands keep working. The pause is
//! the sentinel `<home>/run/paused`, so it survives daemon restarts.

use clap::{Args, Subcommand};
use pmt_core::error::Error;
use serde::Serialize;

use crate::context::Context;
use crate::output::{document, print};

/// `prometheus pause` arguments.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct PauseArgs {
    /// Why (shown in every refusal).
    #[arg(long)]
    pub reason: Option<String>,
    /// Lift or show the pause instead.
    #[command(subcommand)]
    pub command: Option<PauseCommand>,
}

/// `prometheus pause` subcommands.
#[derive(Debug, Subcommand)]
pub enum PauseCommand {
    /// Lift the pause.
    Off,
    /// Print whether the home is paused, why, by whom and since when.
    Status,
}

/// What `pause status --json` prints.
#[derive(Debug, Serialize)]
struct Status {
    paused: bool,
    reason: Option<String>,
    actor: Option<String>,
    since: Option<String>,
}

/// Runs `prometheus pause`.
///
/// # Errors
///
/// `home.io` when the sentinel cannot be written or removed.
pub fn run(args: &PauseArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?;
    let root = home.root();
    match &args.command {
        None => {
            let pause = pmt_core::pause::pause(root, args.reason.as_deref(), "cli")?;
            tracing::warn!("home paused: {}", pause.reason);
            print(&format!(
                "paused: {}; new turns and scheduled jobs are refused until 'prometheus pause off'",
                pause.reason
            ))
        }
        Some(PauseCommand::Off) => {
            if pmt_core::pause::resume(root)? {
                tracing::warn!("home pause lifted");
                print(&"pause lifted")
            } else {
                print(&"not paused")
            }
        }
        Some(PauseCommand::Status) => {
            let pause = pmt_core::pause::active(root);
            if ctx.json {
                let status = Status {
                    paused: pause.is_some(),
                    reason: pause.as_ref().map(|p| p.reason.clone()),
                    actor: pause.as_ref().map(|p| p.actor.clone()),
                    since: pause.as_ref().map(|p| p.at.clone()),
                };
                return document("pause status", &status);
            }
            match pause {
                Some(p) => print(&format!(
                    "paused since {} by {}: {}",
                    p.at, p.actor, p.reason
                )),
                None => print(&"not paused"),
            }
        }
    }
}
