//! `prometheus gateway send` (spec: agent-interop, One-shot send command;
//! messaging-gateway, Home channel and /sethome): one message to a chat
//! without an agent turn. With the daemon running it goes through the
//! profile's durable outbox and the command waits for the delivery; without
//! it, platforms whose adapter can send statelessly send directly with the
//! profile's credentials, and the others need the daemon (exit 69).
//!
//! Exit codes: 0 sent, 1 delivery failure, 2 usage error, 66 unknown target
//! (or no home channel), 69 daemon required, 78 platform not configured.

use std::fmt::Write as _;
use std::io::{IsTerminal as _, Read as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use pmt_core::error::{Error, codes};
use pmt_gateway::messaging::target::{self, Resolved, Target};
use pmt_gateway::messaging::{AdapterSetup, Outgoing, known_targets};
use pmt_rpc::control::{ControlRequest, SendAttachment};
use pmt_rpc::discovery::{Discovery, discover};
use pmt_store::Store;
use pmt_store::gateway::outbox::{OutboundAttachment, OutboundState};
use serde::Serialize;

use super::control_call;
use crate::context::Context;
use crate::output::{document, print};

/// How long the command waits for the daemon to deliver.
const DELIVERY_WAIT: Duration = Duration::from_secs(30);

/// `prometheus gateway send` arguments.
#[derive(Debug, Args)]
pub struct SendArgs {
    /// Where: `platform` (its home channel), `platform:chat_id`,
    /// `platform:chat_id:thread_id` or `platform:#channel-name`.
    #[arg(long, value_name = "TARGET", required_unless_present = "list")]
    pub to: Option<String>,
    /// The message; without it, `--file` or piped stdin.
    pub message: Option<String>,
    /// Read the message from a text file (`-` for stdin).
    #[arg(long, value_name = "PATH", conflicts_with = "message")]
    pub file: Option<String>,
    /// A subject line put before the message.
    #[arg(long)]
    pub subject: Option<String>,
    /// List known targets (home channels, allowed chats, chats with
    /// sessions), optionally of one platform.
    #[arg(long, value_name = "PLATFORM", num_args = 0..=1, default_missing_value = "")]
    pub list: Option<String>,
    /// Print nothing on success.
    #[arg(from_global)]
    pub quiet: bool,
}

/// What `gateway send --json` prints.
#[derive(Debug, Serialize)]
struct SentOutput {
    destination: String,
    /// The outbox entry, when the daemon sent it.
    entry: Option<i64>,
    state: &'static str,
}

fn usage(message: impl Into<String>) -> Error {
    Error::new(&codes::CLI_USAGE, message)
}

/// Runs `prometheus gateway send`.
///
/// # Errors
///
/// As listed in the module documentation.
pub(super) fn run(args: &SendArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    if let Some(platform) = &args.list {
        return list(ctx, Some(platform.as_str()).filter(|p| !p.is_empty()));
    }
    let to = args.to.as_deref().unwrap_or_default();
    let parsed = Target::parse(to)?;
    let body = body(args)?;
    let (text, attachments) = media(&body)?;
    if text.is_empty() && attachments.is_empty() {
        return Err(usage("the message is empty"));
    }
    let home = ctx.home()?;
    let outcome = match discover(home.root())? {
        Discovery::Running(record) => through_daemon(ctx, &record, to, &text, &attachments)?,
        _ => direct(ctx, &parsed, &text, &attachments)?,
    };
    if args.quiet {
        return Ok(0);
    }
    if ctx.json {
        return document("gateway send", &outcome);
    }
    match outcome.entry {
        Some(entry) => print(&format!(
            "sent to {} (outbox entry {entry})",
            outcome.destination
        )),
        None => print(&format!("sent to {}", outcome.destination)),
    }
}

/// The message: the argument, `--file` (text only), or piped stdin.
fn body(args: &SendArgs) -> Result<String, Error> {
    let mut body = match (&args.message, args.file.as_deref()) {
        (Some(message), _) => message.clone(),
        (None, Some("-")) => read_stdin()?,
        (None, Some(path)) => {
            let bytes = std::fs::read(path)
                .map_err(|e| usage(format!("cannot read --file {path}: {e}")))?;
            String::from_utf8(bytes).map_err(|_| {
                usage(format!(
                    "--file {path} is not text; attach files with a MEDIA:<path> line"
                ))
            })?
        }
        (None, None) if !std::io::stdin().is_terminal() => read_stdin()?,
        (None, None) => {
            return Err(usage(
                "no message: give it as an argument, with --file, or on stdin",
            ));
        }
    };
    if let Some(subject) = &args.subject {
        body = format!("{subject}\n\n{body}");
    }
    Ok(body.trim_end().to_owned())
}

fn read_stdin() -> Result<String, Error> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|e| usage(format!("cannot read the message from stdin: {e}")))?;
    Ok(text)
}

/// The text and the `MEDIA:` files, which must exist (made absolute so the
/// daemon finds them).
fn media(body: &str) -> Result<(String, Vec<OutboundAttachment>), Error> {
    let (text, mut attachments) = target::media(body);
    for attachment in &mut attachments {
        let path = Path::new(&attachment.path);
        let absolute = std::path::absolute(path).unwrap_or_else(|_| PathBuf::from(path));
        if !absolute.is_file() {
            return Err(usage(format!(
                "MEDIA:{} is not a readable file",
                attachment.path
            )));
        }
        attachment.path = absolute.to_string_lossy().into_owned();
    }
    Ok((text, attachments))
}

fn control_error(err: &pmt_rpc::control::ControlError) -> Error {
    let code = pmt_core::error::ERROR_CODES
        .iter()
        .find(|c| c.code == err.code)
        .copied()
        .unwrap_or(&codes::GATEWAY_DELIVERY_FAILED);
    Error::new(code, err.message.clone())
}

/// Queues the message in the running daemon's outbox and waits until it
/// is sent or fails.
fn through_daemon(
    ctx: &Context<'_>,
    record: &pmt_rpc::discovery::DaemonRecord,
    to: &str,
    text: &str,
    attachments: &[OutboundAttachment],
) -> Result<SentOutput, Error> {
    let scope = ctx.scope()?;
    let request = ControlRequest::GatewaySend {
        profile: scope.name().as_str().to_owned(),
        target: to.to_owned(),
        text: text.to_owned(),
        attachments: attachments
            .iter()
            .map(|a| SendAttachment {
                path: a.path.clone(),
                as_document: a.as_document,
            })
            .collect(),
    };
    let response = control_call(record, &request)?;
    if let Some(err) = &response.error {
        return Err(control_error(err));
    }
    let queued = response
        .queued
        .ok_or_else(|| Error::new(&codes::RPC_CONNECTION, "the daemon queued nothing"))?;
    let store = Store::open(&scope)?;
    let deadline = std::time::Instant::now() + DELIVERY_WAIT;
    loop {
        let entry = store.outbound(queued.entry)?;
        match entry.as_ref().map(|e| e.state) {
            Some(OutboundState::Sent | OutboundState::Delivered | OutboundState::Read) => {
                return Ok(SentOutput {
                    destination: queued.destination,
                    entry: Some(queued.entry),
                    state: "sent",
                });
            }
            Some(OutboundState::Failed | OutboundState::Unknown) | None => {
                let why = entry
                    .and_then(|e| e.last_error)
                    .unwrap_or_else(|| "removed from the outbox".to_owned());
                return Err(Error::new(
                    &codes::GATEWAY_DELIVERY_FAILED,
                    format!("not delivered to {}: {why}", queued.destination),
                ));
            }
            _ if std::time::Instant::now() > deadline => {
                return Err(Error::new(
                    &codes::GATEWAY_DELIVERY_FAILED,
                    format!(
                        "not delivered to {} within {}s; the outbox keeps retrying (entry {})",
                        queued.destination,
                        DELIVERY_WAIT.as_secs(),
                        queued.entry
                    ),
                ));
            }
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Sends without the daemon through an adapter that can send statelessly.
fn direct(
    ctx: &Context<'_>,
    parsed: &Target,
    text: &str,
    attachments: &[OutboundAttachment],
) -> Result<SentOutput, Error> {
    let scope = ctx.scope()?;
    let gateway = &scope.config().gateway;
    if !target::configured(gateway, &parsed.platform) {
        return Err(target::not_configured(&parsed.platform));
    }
    let to = target::resolve(gateway, parsed)?.ok_or_else(|| {
        let target::Destination::Named(name) = &parsed.destination else {
            unreachable!("only a #name can stay unresolved");
        };
        target::unknown_channel(&parsed.platform, name)
    })?;
    let daemon_required = || {
        Error::new(
            &codes::GATEWAY_DAEMON_REQUIRED,
            format!("daemon required for {}", to.platform),
        )
        .with_hint("start it with 'prometheus daemon start'")
    };
    let factories = super::messaging::factories(ctx);
    let factory = factories.get(&to.platform).ok_or_else(daemon_required)?;
    let setup = AdapterSetup {
        scope: scope.clone(),
        platform: to.platform.clone(),
        config: gateway.platform(&to.platform),
    };
    let adapter = factory(&setup)?;
    if !adapter.describe().stateless_send {
        return Err(daemon_required());
    }
    let message = Outgoing {
        attachments: attachments.to_vec(),
        ..Outgoing::text(&to.chat_id, to.thread_id.as_deref(), text)
    };
    crate::model_cmd::block_on(adapter.send(&message))?.map_err(|err| {
        Error::new(
            &codes::GATEWAY_DELIVERY_FAILED,
            format!("not delivered to {}: {}", to.label(), err.describe()),
        )
    })?;
    Ok(SentOutput {
        destination: Resolved::label(&to),
        entry: None,
        state: "sent",
    })
}

/// `gateway send --list [platform]`.
fn list(ctx: &Context<'_>, platform: Option<&str>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let store = Store::open(&scope)?;
    let targets = known_targets(&scope.config().gateway, &store, platform)?;
    if ctx.json {
        return document(
            "gateway send --list",
            &serde_json::json!({"targets": targets}),
        );
    }
    if targets.is_empty() {
        return print(
            &"no known targets; set a home channel with /sethome or list chats in allow_chats",
        );
    }
    let mut text = String::new();
    for target in targets {
        let name = target.name.map(|n| format!(" ({n})")).unwrap_or_default();
        let _ = writeln!(text, "{}  {}{name}", target.target, target.kind);
    }
    crate::output::print_raw(&text)
}
