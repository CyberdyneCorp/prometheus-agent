//! `prometheus dashboard pair` and `dashboard devices [revoke <name>]`
//! (spec: web-dashboard, Paired device credentials for remote clients):
//! the daemon mints the one-use pairing code (valid 10 minutes, kept in
//! its memory only) on the dashboard listener the device exchanges it on;
//! the device list and revocation work on the daemon's device registry.

use std::fmt::Write as _;

use pmt_core::error::Error;
use pmt_rpc::control::DashboardAction;
use serde_json::json;

use super::ask;
use crate::context::Context;
use crate::daemon_cmd::running_or_start;
use crate::output::{document, print_raw};

/// `dashboard pair` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct PairArgs {
    /// The device's name.
    #[arg(long, default_value = "desktop")]
    pub name: String,
    /// `driver` (default) or `observer`.
    #[arg(long, default_value = "driver")]
    pub role: String,
}

/// `dashboard devices` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum DevicesCommand {
    /// Revoke a device: its credential stops working and its sockets close
    /// with 4401.
    Revoke {
        /// The device's name.
        name: String,
    },
}

/// Runs `dashboard pair`.
///
/// # Errors
///
/// `cli.usage` for a bad name or role, daemon and listener errors.
pub(super) fn pair(args: &PairArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?;
    let (record, _) = running_or_start(home.root(), ctx.profile)?;
    let action = DashboardAction::Pair {
        name: args.name.clone(),
        role: args.role.clone(),
    };
    let state = ask(&record, action)?;
    let code = state.pairing_code.unwrap_or_default();
    if ctx.json {
        return document(
            "dashboard pair",
            &json!({"device": args.name, "role": args.role, "code": code, "url": state.url,
                    "expires_in_secs": pmt_gateway::devices::CODE_TTL.as_secs()}),
        );
    }
    print_raw(&format!(
        "Pairing code for '{}' ({}): {code}\nEnter it in the desktop app with the address {} \
         within 10 minutes; it works once.\n",
        args.name, args.role, state.url
    ))
}

/// Runs `dashboard devices [revoke <name>]`.
///
/// # Errors
///
/// `device.not_found`, daemon and registry errors.
pub(super) fn devices(command: Option<&DevicesCommand>, ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?;
    if let Some(DevicesCommand::Revoke { name }) = command {
        return crate::daemon_cmd::devices_cmd::revoke(home.root(), name, ctx.json);
    }
    let list = pmt_gateway::devices::Devices::open(home.root()).list()?;
    if ctx.json {
        return document("dashboard devices", &json!({"devices": list}));
    }
    if list.is_empty() {
        return print_raw("no paired devices\n");
    }
    let mut text = String::new();
    for device in &list {
        let profiles = if device.profiles.is_empty() {
            "every profile".to_owned()
        } else {
            device.profiles.join(",")
        };
        let _ = writeln!(
            text,
            "{}  {}  {profiles}  paired {}  rotated {}",
            device.name,
            device.role.as_str(),
            device.paired_at,
            device.rotated_at
        );
    }
    print_raw(&text)
}
