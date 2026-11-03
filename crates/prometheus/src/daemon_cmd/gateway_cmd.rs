//! `prometheus gateway status|reload|outbox` and `prometheus pairing`
//! (spec: messaging-gateway, One gateway daemon per home; Documented
//! pairing flow; Durable delivery outbox). Status and reload ask the
//! running daemon; pairing and the outbox work on the selected profile's
//! `state.db`, and a message they queue (a paired sender's welcome, a
//! retried entry) is sent by the daemon's delivery worker.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use clap::Subcommand;
use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_rpc::control::{AdapterReport, ControlRequest};
use pmt_store::Store;
use pmt_store::gateway::outbox::{Outbound, OutboundState};
use pmt_store::gateway::pairing::Pairing;
use serde::Serialize;

use super::{control_call, wait_running};
use crate::context::Context;
use crate::output::{document, print_raw};

/// `prometheus gateway` subcommands.
#[derive(Debug, Subcommand)]
pub enum GatewayCommand {
    /// Print each served profile's messaging adapters and their state.
    Status,
    /// Start the adapters served profiles enable that are not running
    /// (running adapters and conversations are untouched).
    Reload,
    /// Print each loaded adapter's capability descriptor (limits, unit,
    /// media, interactive tier, optional capabilities).
    Platforms,
    /// Stop the daemon that serves this profile's adapters (releasing
    /// their platform credentials), after its running turns finish.
    Stop,
    /// The durable delivery outbox of the selected profile.
    Outbox {
        /// The outbox subcommand.
        #[command(subcommand)]
        command: OutboxCommand,
    },
    /// Send one message to a chat without an agent turn (through the
    /// daemon's outbox when it runs).
    Send(super::send_cmd::SendArgs),
    /// List every running Prometheus daemon of this OS user, whatever its
    /// home.
    List,
    /// Enroll this home with a relay connector using a one-time token.
    Enroll(super::enroll_cmd::EnrollArgs),
    /// Print (or write) a Slack app manifest for Socket Mode with one
    /// slash command per gateway command.
    SlackManifest(super::slack_manifest_cmd::SlackManifestArgs),
}

/// `prometheus gateway outbox` subcommands.
#[derive(Debug, Subcommand)]
pub enum OutboxCommand {
    /// List entries, newest first.
    List {
        /// Only entries in this state (`queued`, `sending`, `sent`,
        /// `delivered`, `read`, `failed`, `unknown`).
        #[arg(long)]
        state: Option<String>,
        /// At most this many.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Queue a `failed` or `unknown` entry again (a resend of an `unknown`
    /// one may duplicate it on the platform).
    Retry {
        /// The entry id from `outbox list`.
        id: i64,
    },
    /// Remove an entry that is not being sent.
    Drop {
        /// The entry id from `outbox list`.
        id: i64,
    },
}

/// `prometheus pairing` subcommands.
///
/// A sender who messages a bot without access gets an 8-character code.
/// A code is approved in one of three ways: `prometheus pairing approve
/// <platform> <code>`, the RPC `pairing.approve` from a client attached to
/// the profile, or the admin-only chat command `/pair approve <code>`.
#[derive(Debug, Subcommand)]
pub enum PairingCommand {
    /// List pending codes (unexpired) and approved senders.
    List {
        /// Only this platform.
        #[arg(long)]
        platform: Option<String>,
    },
    /// Approve a pending code (spaces, dashes and case are ignored); the
    /// sender is told in their DM.
    Approve {
        /// The platform the code was issued on.
        platform: String,
        /// The code the sender got.
        code: String,
    },
    /// Refuse a pending code.
    Deny {
        /// The platform the code was issued on.
        platform: String,
        /// The code the sender got.
        code: String,
    },
    /// Remove an approved sender.
    Revoke {
        /// The platform.
        platform: String,
        /// The sender's platform id (from `pairing list`).
        sender: String,
    },
}

/// Runs `prometheus gateway <command>`.
///
/// # Errors
///
/// `daemon.not_running` for status and reload without a daemon, store and
/// outbox errors.
pub(super) fn run(command: &GatewayCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        GatewayCommand::Status => status(ctx.home()?.root(), ctx, false),
        GatewayCommand::Reload => status(ctx.home()?.root(), ctx, true),
        GatewayCommand::Platforms => platforms(ctx.home()?.root(), ctx),
        GatewayCommand::Stop => super::stop(ctx.home()?.root(), ctx.json),
        GatewayCommand::Outbox { command } => outbox(command, ctx),
        GatewayCommand::Send(args) => super::send_cmd::run(args, ctx),
        GatewayCommand::List => super::inventory::list(ctx),
        GatewayCommand::Enroll(args) => super::enroll_cmd::run(args, ctx),
        GatewayCommand::SlackManifest(args) => super::slack_manifest_cmd::run(args, ctx),
    }
}

/// What `gateway status --json` prints.
#[derive(Debug, Serialize)]
struct StatusOutput {
    profiles: Vec<String>,
    adapters: Vec<AdapterReport>,
}

/// The running daemon's status (after a reload when asked).
fn daemon_status(home: &Path, reload: bool) -> Result<pmt_rpc::control::DaemonStatus, Error> {
    let Some(record) = wait_running(home, Duration::from_secs(2))? else {
        return Err(Error::new(
            &codes::DAEMON_NOT_RUNNING,
            "the gateway daemon is not running",
        )
        .with_hint("start it with 'prometheus daemon start'"));
    };
    let request = if reload {
        ControlRequest::GatewayReload
    } else {
        ControlRequest::Status
    };
    control_call(&record, &request)?
        .status
        .ok_or_else(|| Error::new(&codes::RPC_CONNECTION, "the daemon sent no status"))
}

/// What `gateway platforms --json` prints.
#[derive(Debug, Serialize)]
struct PlatformsOutput {
    platforms: Vec<LoadedPlatform>,
}

/// One loaded adapter's descriptor.
#[derive(Debug, Serialize)]
struct LoadedPlatform {
    profile: String,
    platform: String,
    descriptor: serde_json::Value,
}

/// `gateway platforms`: each loaded adapter's descriptor.
fn platforms(home: &Path, ctx: &Context<'_>) -> Result<u8, Error> {
    let daemon = daemon_status(home, false)?;
    let loaded: Vec<LoadedPlatform> = daemon
        .adapters
        .into_iter()
        .filter_map(|a| {
            a.descriptor.map(|descriptor| LoadedPlatform {
                profile: a.profile,
                platform: a.platform,
                descriptor,
            })
        })
        .collect();
    if ctx.json {
        return document("gateway platforms", &PlatformsOutput { platforms: loaded });
    }
    let mut text = String::new();
    if loaded.is_empty() {
        text.push_str("no messaging adapters loaded\n");
    }
    for adapter in &loaded {
        let _ = writeln!(text, "{}/{}:", adapter.profile, adapter.platform);
        if let Some(fields) = adapter.descriptor.as_object() {
            for (key, value) in fields {
                let _ = writeln!(text, "  {key} = {value}");
            }
        }
    }
    print_raw(&text)
}

fn status(home: &Path, ctx: &Context<'_>, reload: bool) -> Result<u8, Error> {
    let daemon = daemon_status(home, reload)?;
    if ctx.json {
        let output = StatusOutput {
            profiles: daemon.profiles,
            adapters: daemon.adapters,
        };
        return document("gateway status", &output);
    }
    print_raw(&adapters_text(&daemon.profiles, &daemon.adapters))
}

/// Each profile's adapters, one line each.
fn adapters_text(profiles: &[String], adapters: &[AdapterReport]) -> String {
    let mut text = String::new();
    for profile in profiles {
        let _ = writeln!(text, "{profile}:");
        let mine: Vec<&AdapterReport> = adapters.iter().filter(|a| &a.profile == profile).collect();
        if mine.is_empty() {
            text.push_str("  no messaging adapters enabled\n");
        }
        for adapter in mine {
            let _ = writeln!(text, "  {}", adapter_line(adapter));
        }
    }
    text
}

/// `platform: state (code): message [paused] [needs attention]`.
fn adapter_line(adapter: &AdapterReport) -> String {
    let mut line = format!("{}: {}", adapter.platform, adapter.state);
    if let Some(code) = &adapter.code {
        let _ = write!(line, " ({code})");
    }
    if let Some(message) = &adapter.message {
        let _ = write!(line, ": {message}");
    }
    if adapter.paused {
        line.push_str(" [paused]");
    }
    if adapter.attention {
        line.push_str(" [needs attention]");
    }
    line
}

fn store(ctx: &Context<'_>) -> Result<Store, Error> {
    Store::open(&ctx.scope()?)
}

fn outbox(command: &OutboxCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let store = store(ctx)?;
    match command {
        OutboxCommand::List { state, limit } => {
            let state = state
                .as_deref()
                .map(|s| {
                    OutboundState::parse(s).ok_or_else(|| {
                        Error::new(
                            &codes::CLI_USAGE,
                            format!("unknown outbox state '{s}'; use queued, sending, sent, delivered, read, failed or unknown"),
                        )
                    })
                })
                .transpose()?;
            let entries = store.outbox_entries(state, *limit)?;
            list_outbox(&entries, ctx.json)
        }
        OutboxCommand::Retry { id } => {
            store.requeue_outbound(*id)?;
            eprintln!("outbox entry {id} queued again");
            Ok(0)
        }
        OutboxCommand::Drop { id } => {
            store.drop_outbound(*id)?;
            eprintln!("outbox entry {id} dropped");
            Ok(0)
        }
    }
}

#[derive(Debug, Serialize)]
struct OutboxEntry<'a> {
    id: i64,
    state: &'static str,
    kind: &'static str,
    platform: &'a str,
    chat_id: &'a str,
    session_id: Option<&'a str>,
    attempts: u32,
    created_at: String,
    last_error: Option<&'a str>,
}

fn list_outbox(entries: &[Outbound], json: bool) -> Result<u8, Error> {
    let rows: Vec<OutboxEntry<'_>> = entries
        .iter()
        .map(|e| OutboxEntry {
            id: e.id,
            state: e.state.as_str(),
            kind: e.kind.as_str(),
            platform: &e.platform,
            chat_id: &e.chat_id,
            session_id: e.session_id.as_deref(),
            attempts: e.attempts,
            created_at: e.created_at.to_string(),
            last_error: e.last_error.as_deref(),
        })
        .collect();
    if json {
        return document("gateway outbox list", &serde_json::json!({"entries": rows}));
    }
    let mut text = String::new();
    for row in &rows {
        let _ = write!(
            text,
            "{}  {}  {}  {}:{}  attempts {}  {}",
            row.id, row.state, row.kind, row.platform, row.chat_id, row.attempts, row.created_at
        );
        if let Some(error) = row.last_error {
            let _ = write!(text, "  ({error})");
        }
        text.push('\n');
    }
    print_raw(&text)
}

/// Runs `prometheus pairing <command>`.
///
/// # Errors
///
/// `gateway.pairing_locked`, `gateway.pairing_unknown`,
/// `gateway.pairing_not_found`, store errors.
pub(super) fn pairing(command: &PairingCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let store = store(ctx)?;
    match command {
        PairingCommand::List { platform } => {
            list_pairings(&store.pairings(platform.as_deref())?, ctx.json)
        }
        PairingCommand::Approve { platform, code } => {
            let pairing = store.approve_pairing(platform, code, Timestamp::now())?;
            store.enqueue_outbound(&pmt_gateway::messaging::paired_notice(&pairing))?;
            eprintln!(
                "approved {} on {}; they are told in their DM",
                who(&pairing),
                pairing.platform
            );
            Ok(0)
        }
        PairingCommand::Deny { platform, code } => {
            let pairing = store.deny_pairing(platform, code, Timestamp::now())?;
            eprintln!(
                "denied the pairing request of {} on {platform}",
                who(&pairing)
            );
            Ok(0)
        }
        PairingCommand::Revoke { platform, sender } => {
            store.revoke_pairing(platform, sender)?;
            eprintln!("revoked {sender} on {platform}");
            Ok(0)
        }
    }
}

fn who(pairing: &Pairing) -> String {
    match &pairing.sender_name {
        Some(name) => format!("{name} ({})", pairing.sender_id),
        None => pairing.sender_id.clone(),
    }
}

#[derive(Debug, Serialize)]
struct PairingEntry<'a> {
    platform: &'a str,
    sender_id: &'a str,
    sender_name: Option<&'a str>,
    state: &'static str,
    requested_at: String,
    expires_at: Option<String>,
    approved_at: Option<String>,
}

fn list_pairings(entries: &[Pairing], json: bool) -> Result<u8, Error> {
    let rows: Vec<PairingEntry<'_>> = entries
        .iter()
        .map(|p| PairingEntry {
            platform: &p.platform,
            sender_id: &p.sender_id,
            sender_name: p.sender_name.as_deref(),
            state: if p.approved { "approved" } else { "pending" },
            requested_at: p.requested_at.to_string(),
            expires_at: p.expires_at.map(|t| t.to_string()),
            approved_at: p.approved_at.map(|t| t.to_string()),
        })
        .collect();
    if json {
        return document("pairing list", &serde_json::json!({"pairings": rows}));
    }
    let mut text = String::new();
    for row in &rows {
        let _ = write!(text, "{}  {}  {}", row.platform, row.state, row.sender_id);
        if let Some(name) = row.sender_name {
            let _ = write!(text, " ({name})");
        }
        match (&row.expires_at, &row.approved_at) {
            (Some(expires), _) => {
                let _ = write!(text, "  expires {expires}");
            }
            (_, Some(approved)) => {
                let _ = write!(text, "  approved {approved}");
            }
            _ => {}
        }
        text.push('\n');
    }
    print_raw(&text)
}
