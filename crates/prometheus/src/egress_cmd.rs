//! `prometheus egress` (spec: security-boundaries, Egress proxy operation
//! and status; Egress credential source and placeholder rotation): the
//! profile's egress proxy for container and remote sandboxes.

use std::fmt::Write as _;

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_core::error::Error;
use serde_json::{Value, json};

use crate::context::Context;
use crate::egress::{gateway, mappings};
use crate::output::{document, print_raw};
use crate::prompt;

/// `prometheus egress` subcommands.
#[derive(Debug, Subcommand)]
pub enum EgressCommand {
    /// Listeners, mode, credential mappings (placeholders redacted), denied
    /// connections and provider secrets injection does not cover.
    Status {
        /// Show the placeholders in full.
        #[arg(long)]
        show_tokens: bool,
    },
    /// Start the proxy (the daemon starts it for profiles that inject).
    Start,
    /// Stop it (forced after 5 seconds).
    Stop,
    /// Stop and start it.
    Restart,
    /// Apply allowlist, denylist and mapping changes without dropping
    /// connections.
    Reload,
    /// Mint new placeholders for every mapping (running sandboxes get them
    /// on their next command).
    Rotate {
        /// Do not ask to type `rotate`.
        #[arg(long)]
        confirm: bool,
    },
    /// Run the proxy in this process (started by `start` and the daemon).
    #[command(hide = true)]
    Serve {
        /// End when stdin closes (the daemon owns the proxy).
        #[arg(long)]
        owned: bool,
    },
}

/// Runs one `egress` subcommand.
///
/// # Errors
///
/// `egress.proxy_start`, `egress.proxy_down`, `home.io`.
pub fn run(command: &EgressCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    match command {
        EgressCommand::Status { show_tokens } => status(&scope, ctx.json, *show_tokens),
        EgressCommand::Start => start(&scope),
        EgressCommand::Stop => {
            let stopped = gateway::stop(&scope)?;
            eprintln!(
                "{}",
                if stopped {
                    "egress proxy stopped"
                } else {
                    "egress proxy was not running"
                }
            );
            Ok(0)
        }
        EgressCommand::Restart => {
            gateway::stop(&scope)?;
            start(&scope)
        }
        EgressCommand::Reload => reload(&scope),
        EgressCommand::Rotate { confirm } => rotate(&scope, *confirm),
        EgressCommand::Serve { owned } => gateway::serve::serve(&scope, *owned),
    }
}

fn start(scope: &ProfileScope) -> Result<u8, Error> {
    let (record, child) = gateway::start(scope, false)?;
    // Detached: it keeps running after this command.
    drop(child);
    eprintln!(
        "egress proxy started (pid {}): loopback {}, containers {}",
        record.pid, record.loopback, record.bridge
    );
    Ok(0)
}

fn reload(scope: &ProfileScope) -> Result<u8, Error> {
    let Some((record, _)) = gateway::running(scope) else {
        return Err(gateway::down());
    };
    gateway::ask(&record, "reload").map_err(|why| {
        Error::new(
            &pmt_core::error::codes::EGRESS_PROXY_START,
            format!("the egress proxy did not reload: {why}"),
        )
    })?;
    eprintln!("egress proxy reloaded");
    Ok(0)
}

fn rotate(scope: &ProfileScope, confirm: bool) -> Result<u8, Error> {
    if !confirm {
        let what = "rotating invalidates the placeholders running sandboxes hold";
        match prompt::confirm_typed(what, "rotate")? {
            None => {
                eprintln!("error: rotation needs confirmation; rerun with --confirm");
                return Ok(pmt_core::exit::APPROVAL_REQUIRED);
            }
            Some(false) => {
                eprintln!("not rotated");
                return Ok(1);
            }
            Some(true) => {}
        }
    }
    let backup = mappings::rotate(scope)?;
    if let Some((record, _)) = gateway::running(scope) {
        let _ = gateway::ask(&record, "reload");
    }
    match backup {
        Some(path) => eprintln!(
            "new placeholders minted; previous mappings kept in {}",
            path.display()
        ),
        None => eprintln!("new placeholders minted"),
    }
    Ok(0)
}

fn status(scope: &ProfileScope, json: bool, show_tokens: bool) -> Result<u8, Error> {
    let running = gateway::running(scope);
    let placeholders = running.as_ref().map_or_else(
        || mappings::read(&mappings::path(scope)),
        |(_, s)| serde_json::from_value(s["placeholders"].clone()).unwrap_or_default(),
    );
    let shown: serde_json::Map<String, Value> = placeholders
        .iter()
        .map(|(name, token)| {
            let token = if show_tokens {
                token.clone()
            } else {
                mappings::redacted(token)
            };
            (name.clone(), json!(token))
        })
        .collect();
    let report = json!({
        "running": running.is_some(),
        "pid": running.as_ref().map(|(r, _)| r.pid),
        "loopback": running.as_ref().map(|(r, _)| r.loopback.to_string()),
        "bridge": running.as_ref().map(|(r, _)| r.bridge.to_string()),
        "mode": scope.config().security.egress.mode.as_str(),
        "credential_injection": scope.config().security.egress.credential_injection,
        "mappings": shown,
        "denied": running.as_ref().and_then(|(_, s)| s["denied"].as_u64()).unwrap_or(0),
        "uncovered": uncovered(scope),
    });
    if json {
        return document("egress status", &report);
    }
    print_raw(&describe(&report))
}

/// Provider secrets the profile holds that injection does not map.
fn uncovered(scope: &ProfileScope) -> Vec<String> {
    let config = scope.config();
    let Ok(registry) = pmt_providers::registry::ProviderRegistry::for_profile(config, Vec::new())
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = registry
        .iter()
        .flat_map(pmt_providers::Provider::secret_names)
        .filter(|name| !config.security.egress.inject.contains_key(name))
        .filter(|name| scope.secrets().resolve(name).is_ok())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn describe(report: &Value) -> String {
    let mut out = String::new();
    if report["running"] == true {
        let _ = writeln!(
            out,
            "egress proxy: running (pid {})\nloopback: {}\ncontainers: {}",
            report["pid"],
            report["loopback"].as_str().unwrap_or_default(),
            report["bridge"].as_str().unwrap_or_default()
        );
    } else {
        out.push_str("egress proxy: not running\n");
    }
    let _ = writeln!(
        out,
        "mode: {}\ncredential injection: {}\ndenied connections: {}",
        report["mode"].as_str().unwrap_or_default(),
        if report["credential_injection"] == true {
            "on"
        } else {
            "off"
        },
        report["denied"]
    );
    for (name, token) in report["mappings"].as_object().into_iter().flatten() {
        let _ = writeln!(
            out,
            "mapping: {name} -> {}",
            token.as_str().unwrap_or_default()
        );
    }
    for name in report["uncovered"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "not injected: {} (sandboxes would get its real value)",
            name.as_str().unwrap_or_default()
        );
    }
    out
}

/// The line `/egress status` adds when the profile injects credentials.
#[must_use]
pub fn proxy_line(scope: &ProfileScope) -> Option<String> {
    if !gateway::covers(scope) {
        return None;
    }
    Some(match gateway::running(scope) {
        Some((record, status)) => format!(
            "egress proxy: running (pid {}), containers at {}, {} denied",
            record.pid, record.bridge, status["denied"]
        ),
        None => "egress proxy: not running".to_owned(),
    })
}
