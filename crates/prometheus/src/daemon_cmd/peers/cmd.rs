//! `prometheus gateway peer` (spec: agent-interop, Peer gateway registry;
//! Peer direct messages and remote runs). A peer is another gateway's API
//! server; its key is read from stdin into this profile's secrets store
//! (never from argv), and a dm or run goes to the remote agent's canonical
//! Bot Chat. Exit codes: 0 done, 1 the peer refused, 2 usage, 66 unknown
//! peer or agent, 69 unreachable while connecting, 78 key missing or
//! rejected.

use std::fmt::Write as _;
use std::io::Read as _;
use std::time::{Duration, Instant};

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_config::schema::{PeerGateway, SecretName, peer_key_secret, valid_peer_name};
use pmt_core::error::{Error, codes};
use serde_json::{Value, json};

use super::client::{Peer, PeerError, invalid_name};
use crate::context::Context;
use crate::model_cmd::block_on;
use crate::output::{document, print};

/// How long a dm waits for the remote turn before reporting it running.
const DM_WAIT: Duration = Duration::from_mins(5);
/// The run states that end a run.
const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "interrupted"];

/// `prometheus gateway peer` subcommands.
#[derive(Debug, Subcommand)]
pub enum PeerCommand {
    /// Register a peer gateway; its API server key is read from stdin (or
    /// a hidden prompt), never from the command line.
    Add {
        /// The peer's name (`^[a-z][a-z0-9-]{0,31}$`).
        name: String,
        /// The peer's API server base URL (`http://spark.lan:8642`).
        #[arg(long)]
        url: String,
        /// A note `peer list` shows.
        #[arg(long)]
        note: Option<String>,
    },
    /// List registered peers with their last probe.
    List,
    /// Remove a peer and its stored key.
    Remove {
        /// The peer.
        name: String,
    },
    /// Send one message to a remote agent's Bot Chat and print its reply
    /// (message from the argument, else stdin read verbatim).
    Dm {
        /// `<peer>` or `<peer>/<agent>`.
        target: String,
        /// The message.
        message: Option<String>,
    },
    /// Submit an asynchronous run in a remote agent's Bot Chat and print
    /// its run id (a retried key returns the same run).
    Run {
        /// `<peer>` or `<peer>/<agent>`.
        target: String,
        /// The message (else stdin read verbatim).
        message: Option<String>,
        /// Makes retries return the first run instead of starting another.
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Print a remote run.
    Status {
        /// `<peer>` or `<peer>/<agent>`.
        peer: String,
        /// The run id.
        run_id: String,
    },
    /// Stop a remote run.
    Stop {
        /// `<peer>` or `<peer>/<agent>`.
        peer: String,
        /// The run id.
        run_id: String,
    },
}

/// Runs `prometheus gateway peer <command>`.
///
/// # Errors
///
/// As each subcommand.
pub(crate) fn run(command: &PeerCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    match command {
        PeerCommand::Add { name, url, note } => add(ctx, &scope, name, url, note.as_deref()),
        PeerCommand::List => list(ctx, &scope),
        PeerCommand::Remove { name } => remove(ctx, &scope, name),
        PeerCommand::Dm { target, message } => dm(ctx, &scope, target, message.as_deref()),
        PeerCommand::Run {
            target,
            message,
            idempotency_key,
        } => submit(
            ctx,
            &scope,
            target,
            message.as_deref(),
            idempotency_key.as_deref(),
        ),
        PeerCommand::Status { peer, run_id } => status(ctx, &scope, peer, run_id, false),
        PeerCommand::Stop { peer, run_id } => status(ctx, &scope, peer, run_id, true),
    }
}

fn add(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    name: &str,
    url: &str,
    note: Option<&str>,
) -> Result<u8, Error> {
    if !valid_peer_name(name) {
        return Err(invalid_name(name));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(Error::new(
            &codes::CLI_USAGE,
            format!("--url '{url}' is not an http(s) URL"),
        ));
    }
    let secret = peer_key_secret(name);
    let key = crate::prompt::read_secret(&secret)?;
    let entry = PeerGateway {
        url: url.trim_end_matches('/').to_owned(),
        note: note.map(str::to_owned),
        key_secret: SecretName(secret.clone()),
    };
    let peer = Peer::new(scope, name, &entry, key.clone())?;
    let probe = match block_on(peer.probe())? {
        Ok(_) => (true, "ok".to_owned()),
        Err(PeerError::KeyRejected) => return Err(PeerError::KeyRejected.into_error(name)),
        Err(PeerError::Unreachable(why)) => {
            eprintln!("warning: peer {name} unreachable: {why}; registered anyway");
            (false, format!("unreachable: {why}"))
        }
        Err(PeerError::NotFound(why) | PeerError::Rejected(_, why)) => {
            eprintln!("warning: peer {name} answered the probe with an error: {why}");
            (false, why)
        }
    };
    scope.secrets().store().set(&secret, &key)?;
    scope.profile().config_file().put_peer(name, &entry)?;
    record_probe(scope, name, probe.0, &probe.1);
    let body = json!({"name": name, "url": entry.url, "note": entry.note, "key_secret": secret,
        "probe": {"ok": probe.0, "detail": probe.1}});
    if ctx.json {
        return document("gateway peer add", &body);
    }
    print(&format!(
        "peer {name} registered ({}; probe: {})",
        entry.url, probe.1
    ))
}

fn record_probe(scope: &ProfileScope, name: &str, ok: bool, detail: &str) {
    if let Err(err) =
        pmt_store::Store::open(scope).and_then(|s| s.record_peer_probe(name, ok, detail))
    {
        tracing::warn!("the probe of peer {name} was not recorded: {err}");
    }
}

fn list(ctx: &Context<'_>, scope: &ProfileScope) -> Result<u8, Error> {
    let store = pmt_store::Store::open(scope).ok();
    let rows: Vec<Value> = scope
        .config()
        .peers
        .iter()
        .map(|(name, peer)| {
            let probe = store.as_ref().and_then(|s| s.peer_probe(name).ok().flatten());
            json!({"name": name, "url": peer.url, "note": peer.note, "key_secret": peer.key_secret.0,
                "last_probe": probe.map(|p| json!({"at": p.at.to_string(), "ok": p.ok, "detail": p.detail}))})
        })
        .collect();
    if ctx.json {
        return document("gateway peer list", &json!({"peers": rows}));
    }
    if rows.is_empty() {
        return print(&"no peers (prometheus gateway peer add <name> --url <url>)");
    }
    let mut text = String::new();
    for row in &rows {
        let probe = row["last_probe"]["detail"]
            .as_str()
            .unwrap_or("never probed");
        let note = row["note"]
            .as_str()
            .map(|n| format!("  ({n})"))
            .unwrap_or_default();
        let _ = writeln!(
            text,
            "{}  {}  last probe: {probe}{note}",
            row["name"].as_str().unwrap_or_default(),
            row["url"].as_str().unwrap_or_default()
        );
    }
    crate::output::print_raw(&text)
}

fn remove(ctx: &Context<'_>, scope: &ProfileScope, name: &str) -> Result<u8, Error> {
    if !valid_peer_name(name) {
        return Err(invalid_name(name));
    }
    let Some(entry) = scope.config().peers.get(name).cloned() else {
        return Err(Error::new(
            &codes::PEER_NOT_FOUND,
            format!("no peer named {name}"),
        ));
    };
    scope.profile().config_file().remove_peer(name)?;
    scope.secrets().store().remove(&entry.key_secret.0)?;
    if let Ok(store) = pmt_store::Store::open(scope) {
        let _ = store.forget_peer_probe(name);
    }
    if ctx.json {
        return document(
            "gateway peer remove",
            &json!({"name": name, "removed": true}),
        );
    }
    print(&format!("peer {name} removed"))
}

/// `<peer>[/<agent>]`.
fn target(scope: &ProfileScope, text: &str) -> Result<(Peer, Option<String>), Error> {
    let (peer, agent) = match text.split_once('/') {
        Some((peer, agent)) if !agent.is_empty() => (peer, Some(agent.to_owned())),
        _ => (text, None),
    };
    Ok((Peer::of(scope, peer)?, agent))
}

/// The message argument, else stdin verbatim.
fn message_of(arg: Option<&str>) -> Result<String, Error> {
    if let Some(text) = arg {
        return Ok(text.to_owned());
    }
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text).map_err(|e| {
        Error::new(
            &codes::CLI_USAGE,
            format!("cannot read the message from stdin: {e}"),
        )
    })?;
    if text.is_empty() {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "no message: pass it as an argument or on stdin",
        ));
    }
    Ok(text)
}

/// Ensures the agent's Bot Chat and submits `message` there; the run.
async fn start(
    peer: &Peer,
    agent: Option<&str>,
    message: &str,
    key: &str,
) -> Result<Value, PeerError> {
    let session = peer.ensure_bot_chat(agent).await?;
    peer.run(agent, &session, message, key).await
}

fn dm(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    target_text: &str,
    message: Option<&str>,
) -> Result<u8, Error> {
    let (peer, agent) = target(scope, target_text)?;
    let message = message_of(message)?;
    let key = format!("dm-{}", ulid::Ulid::generate());
    let outcome = block_on(async {
        let run = start(&peer, agent.as_deref(), &message, &key).await?;
        let run_id = run["run_id"].as_str().unwrap_or_default().to_owned();
        Ok::<_, PeerError>((
            run_id.clone(),
            follow(&peer, agent.as_deref(), &run_id).await,
        ))
    })?;
    let (run_id, finished) = outcome.map_err(|e| e.into_error(&peer.name))?;
    let Some(run) = finished else {
        let text = format!(
            "message delivered to {}'s Bot Chat; turn still running (run {run_id})",
            peer.name
        );
        if ctx.json {
            return document(
                "gateway peer dm",
                &json!({"peer": peer.name, "run_id": run_id, "status": "running", "reply": Value::Null, "note": text}),
            );
        }
        return print(&text);
    };
    let status = run["status"].as_str().unwrap_or_default();
    if status != "completed" {
        let why = run["error"].as_str().unwrap_or(status);
        return Err(
            PeerError::Rejected(200, format!("the turn {status}: {why}")).into_error(&peer.name),
        );
    }
    let reply = run["output"].as_str().unwrap_or_default();
    if ctx.json {
        return document(
            "gateway peer dm",
            &json!({"peer": peer.name, "run_id": run_id, "status": status, "reply": reply}),
        );
    }
    print(&reply)
}

/// Polls run `run_id` until it ends; `None` when it still runs after
/// [`DM_WAIT`] or the connection was lost after it was accepted (the turn
/// runs on: resending would run it twice).
async fn follow(peer: &Peer, agent: Option<&str>, run_id: &str) -> Option<Value> {
    let started = Instant::now();
    let mut delay = Duration::from_millis(200);
    while started.elapsed() < DM_WAIT {
        match peer.status(agent, run_id).await {
            Ok(run) if TERMINAL.contains(&run["status"].as_str().unwrap_or_default()) => {
                return Some(run);
            }
            Ok(_) => {}
            Err(_) => return None,
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
    None
}

fn submit(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    target_text: &str,
    message: Option<&str>,
    key: Option<&str>,
) -> Result<u8, Error> {
    let (peer, agent) = target(scope, target_text)?;
    let message = message_of(message)?;
    let key = key.map_or_else(|| format!("run-{}", ulid::Ulid::generate()), str::to_owned);
    let run = block_on(start(&peer, agent.as_deref(), &message, &key))?
        .map_err(|e| e.into_error(&peer.name))?;
    let run_id = run["run_id"].as_str().unwrap_or_default();
    if ctx.json {
        return document(
            "gateway peer run",
            &json!({"peer": peer.name, "run_id": run_id, "status": run["status"], "idempotency_key": key}),
        );
    }
    print(&run_id)
}

fn status(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    target_text: &str,
    run_id: &str,
    stop: bool,
) -> Result<u8, Error> {
    let (peer, agent) = target(scope, target_text)?;
    let answer = if stop {
        block_on(peer.stop(agent.as_deref(), run_id))?
    } else {
        block_on(peer.status(agent.as_deref(), run_id))?
    }
    .map_err(|e| e.into_error(&peer.name))?;
    let command = if stop {
        "gateway peer stop"
    } else {
        "gateway peer status"
    };
    let state = answer["status"].as_str().unwrap_or_default().to_owned();
    let body = json!({"peer": peer.name, "run_id": run_id, "status": state, "output": answer["output"], "error": answer["error"]});
    if ctx.json {
        return document(command, &body);
    }
    match answer["output"].as_str().filter(|_| !stop) {
        Some(output) => print(&format!("{run_id}: {state}\n{output}")),
        None => print(&format!("{run_id}: {state}")),
    }
}
