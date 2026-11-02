//! `prometheus rpc` (spec: workspace-architecture, Contracts generated from
//! types and checked in; RPC debug client): `rpc schema` prints the
//! client-protocol schema of the running binary (the checked-in
//! `schema/rpc.json`); `rpc call` and `rpc events` talk to the home's
//! running daemon as a `cli` client of the selected profile.

use std::io::Write as _;

use clap::Subcommand;
use pmt_core::error::{Error, codes};
use pmt_rpc::RpcError;
use pmt_rpc::client::{Client, error_of};
use pmt_rpc::contracts::{ClientKind, Envelope, EventsSinceParams, HelloParams};
use pmt_rpc::discovery::DaemonRecord;
use serde_json::Value;

use crate::context::Context;

/// `prometheus rpc` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum RpcCommand {
    /// Print the client-protocol schema (methods, server requests and
    /// events, each with its owning capability and contract).
    Schema,
    /// Perform the handshake with the running daemon, send one request and
    /// print its result as JSON (`rpc.hello` prints the handshake answer
    /// for the given hello params). A call whose params name a
    /// `session_id` first attaches the connection to that session.
    Call {
        /// The method, for example `session.list`.
        method: String,
        /// Its params as a JSON object (default `{}`).
        params: Option<String>,
    },
    /// Stream the daemon's event frames as JSON lines until interrupted.
    Events {
        /// Attach to this session and stream its events too.
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// With --session: first replay the kept events after this
        /// sequence number (0 replays every kept event).
        #[arg(long, value_name = "SEQ", requires = "session")]
        since: Option<u64>,
    },
}

/// Runs `prometheus rpc <command>`.
///
/// # Errors
///
/// `output.io` when stdout cannot be written; `daemon.not_running` (69),
/// connection and ticket errors for `call` and `events`.
pub fn run(command: &RpcCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        RpcCommand::Schema => {
            let schema = pmt_rpc::Catalog::builtin().schema();
            crate::output::print_raw(&pmt_config::catalog::pretty(&schema))
        }
        RpcCommand::Call { method, params } => {
            let params = parse_params(params.as_deref())?;
            let (record, profile) = target(ctx)?;
            crate::model_cmd::block_on(call(&record, &profile, method, params))?
        }
        RpcCommand::Events { session, since } => {
            let (record, profile) = target(ctx)?;
            let session = session.as_deref();
            crate::model_cmd::block_on(events(&record, &profile, session, *since))?
        }
    }
}

/// The running daemon and the selected profile.
fn target(ctx: &Context<'_>) -> Result<(DaemonRecord, String), Error> {
    let home = ctx.home()?;
    let profile = home.select_profile(ctx.profile)?;
    let record = crate::daemon_client::require(home.root())?;
    Ok((record, profile.as_str().to_owned()))
}

fn parse_params(text: Option<&str>) -> Result<Value, Error> {
    let Some(text) = text else {
        return Ok(Value::Object(serde_json::Map::new()));
    };
    serde_json::from_str(text).map_err(|e| {
        Error::new(
            &codes::CLI_USAGE,
            format!("the params are not valid JSON: {e}"),
        )
        .with_hint("pass one JSON object, for example '{\"limit\": 5}'")
    })
}

async fn call(
    record: &DaemonRecord,
    profile: &str,
    method: &str,
    params: Value,
) -> Result<u8, Error> {
    if method == "rpc.hello" {
        // The handshake is the first request of every connection; calling
        // it means performing it with these params.
        let hello: HelloParams = if params.as_object().is_some_and(serde_json::Map::is_empty) {
            crate::daemon_client::hello(ClientKind::Cli, false)
        } else {
            serde_json::from_value(params).map_err(|e| {
                Error::new(&codes::CLI_USAGE, format!("invalid rpc.hello params: {e}"))
            })?
        };
        let (client, _inbox) = crate::daemon_client::connect_cli(record, profile, &hello).await?;
        client.close();
        return print_pretty(&serde_json::to_value(client.hello()).unwrap_or_default());
    }
    let hello = crate::daemon_client::hello(ClientKind::Cli, false);
    let (client, _inbox) = crate::daemon_client::connect_cli(record, profile, &hello).await?;
    let outcome = match attach_first(&client, method, &params).await {
        Ok(()) => client.call(method, params).await,
        Err(err) => Err(err),
    };
    client.close();
    match outcome {
        Ok(result) => print_pretty(&result),
        Err(err) => Ok(print_rpc_error(method, &err)),
    }
}

/// Session-addressed methods need the connection attached to the session
/// (spec: tui, Session access requires an attachment): a one-request
/// debug connection attaches first, unless the call itself attaches,
/// resumes or detaches.
async fn attach_first(client: &Client, method: &str, params: &Value) -> Result<(), RpcError> {
    const ATTACHING: &[&str] = &["session.attach", "session.resume", "session.detach"];
    let Some(session) = params.get("session_id").and_then(Value::as_str) else {
        return Ok(());
    };
    if ATTACHING.contains(&method) {
        return Ok(());
    }
    let attach = serde_json::json!({ "session_id": session });
    client.call("session.attach", attach).await.map(|_| ())
}

/// Prints `{"error": <the JSON-RPC error object>}` on stderr and returns
/// the exit status of its kind; an unknown method gets a did-you-mean hint.
fn print_rpc_error(method: &str, err: &RpcError) -> u8 {
    let document = serde_json::json!({ "error": err });
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(
        stderr,
        "{}",
        serde_json::to_string_pretty(&document).unwrap_or_default()
    );
    if err.code == pmt_rpc::codes::METHOD_NOT_FOUND {
        let names = pmt_rpc::catalog::INDEX
            .iter()
            .filter(|e| e.kind == pmt_rpc::Kind::Method)
            .map(|e| e.name);
        let found = pmt_core::registry::closest(method, names);
        if !found.is_empty() {
            let _ = writeln!(
                stderr,
                "hint: did you mean {}?",
                pmt_core::registry::or_list(&found)
            );
        }
    }
    error_of(err).exit_code()
}

fn print_pretty(value: &Value) -> Result<u8, Error> {
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    crate::output::print(&text)
}

async fn events(
    record: &DaemonRecord,
    profile: &str,
    session: Option<&str>,
    since: Option<u64>,
) -> Result<u8, Error> {
    // Listening before anything is printed: a SIGINT right after the
    // first line must stop the stream, not kill the process.
    let mut signals = crate::chat::signals::Signals::listen();
    let hello = crate::daemon_client::hello(ClientKind::Cli, false);
    let (client, mut inbox) = crate::daemon_client::connect_cli(record, profile, &hello).await?;
    let mut seen = Watermark::default();
    if let Some(session) = session {
        let params = serde_json::json!({ "session_id": session });
        client
            .call("session.attach", params)
            .await
            .map_err(|e| error_of(&e))?;
        if let Some(last_seen) = since {
            replay(&client, session, last_seen, &mut seen).await?;
        }
    }
    loop {
        tokio::select! {
            event = inbox.events.recv() => match event {
                Some(event) => seen.print(&event)?,
                None => return Err(closed(&client).await),
            },
            code = signals.next() => {
                client.close();
                return Ok(code);
            }
        }
    }
}

/// Prints the kept events of `session` after `last_seen`.
async fn replay(
    client: &Client,
    session: &str,
    last_seen: u64,
    seen: &mut Watermark,
) -> Result<(), Error> {
    let params = EventsSinceParams {
        session_id: session.to_owned(),
        last_seen,
    };
    let kept = client
        .call_typed::<pmt_rpc::contracts::SessionEventsSince>(&params)
        .await
        .map_err(|e| error_of(&e))?;
    for event in &kept.events {
        seen.print(event)?;
    }
    Ok(())
}

async fn closed(client: &Client) -> Error {
    let closed = client.wait_closed().await;
    let detail = closed.code.map_or_else(
        || closed.reason.clone(),
        |c| format!("{c} ({})", closed.reason),
    );
    Error::new(
        &codes::RPC_CONNECTION,
        format!("the daemon closed the connection: {detail}"),
    )
}

/// The newest sequence number printed per session, so an event both
/// replayed and received live prints once.
#[derive(Default)]
struct Watermark(std::collections::HashMap<Option<String>, u64>);

impl Watermark {
    fn print(&mut self, event: &Envelope) -> Result<(), Error> {
        let newest = self.0.entry(event.session_id.clone()).or_insert(0);
        if event.session_id.is_some() && event.seq <= *newest {
            return Ok(());
        }
        *newest = event.seq;
        let mut stdout = std::io::stdout().lock();
        crate::output::write_json(&mut stdout, event)
            .and_then(|()| stdout.flush())
            .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot write stdout: {e}")))
    }
}
