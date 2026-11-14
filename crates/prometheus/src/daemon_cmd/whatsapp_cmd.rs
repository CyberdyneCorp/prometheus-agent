//! `prometheus gateway whatsapp pair [--pairing-code <phone>]` (spec:
//! platform-adapters, `WhatsApp` pairing and modes): links this profile to
//! `WhatsApp` as a linked device. It installs the managed bridge runtime
//! when needed (the pinned Node runtime, then the shipped bridge with its
//! locked packages), replaces any earlier session under
//! `<profile>/platforms/whatsapp/session/` (mode 0700), and runs the
//! bridge in pairing mode: each QR code it reports is drawn in the
//! terminal (60 columns at least, else exit 2 offering `--pairing-code`)
//! until the phone links, at most 120 seconds (then exit 75); with
//! `--pairing-code` the phone links by typing the code shown instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Subcommand};
use pmt_core::error::{Error, codes};
use serde_json::Value;
use tokio::io::AsyncBufReadExt as _;

use crate::context::Context;

/// Columns a QR code needs.
const MIN_COLUMNS: u16 = 60;
/// How long pairing may take.
const PAIR_WINDOW: Duration = Duration::from_mins(2);

/// `prometheus gateway whatsapp`.
#[derive(Debug, Clone, Args)]
pub struct WhatsappArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub command: WhatsappCommand,
}

/// `gateway whatsapp` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum WhatsappCommand {
    /// Link this profile to `WhatsApp`; replaces an earlier link.
    #[command(
        about = "Link this profile to WhatsApp (scan the QR code, or type a pairing code on the phone); replaces an earlier link"
    )]
    Pair {
        /// Link with an 8-character code for this phone number (with
        /// country code) instead of a QR code.
        #[arg(long, value_name = "PHONE")]
        pairing_code: Option<String>,
    },
}

/// Runs `gateway whatsapp ...`.
///
/// # Errors
///
/// `cli.usage` (2) for a narrow terminal; runtime install errors; the
/// bridge's refusal; `gateway.pairing_timeout` (75) when nothing linked in
/// time.
pub(super) fn run(args: &WhatsappArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let WhatsappCommand::Pair { pairing_code } = &args.command;
    if pairing_code.is_none() {
        let columns = columns();
        if columns < MIN_COLUMNS {
            return Err(Error::new(
                &codes::CLI_USAGE,
                format!(
                    "the terminal is {columns} columns wide and the QR code needs {MIN_COLUMNS}; widen it or link with --pairing-code <phone>"
                ),
            ));
        }
    }
    let scope = ctx.scope()?;
    let bridge = bridge(ctx)?;
    let command = link_command(&scope, &bridge, pairing_code.as_deref())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot start the runtime: {e}")))?;
    let never = pmt_core::cancel::CancelToken::new();
    let linked = runtime.block_on(pair(command, pair_window(), &never, &mut show))?;
    let profile = scope.name();
    crate::output::print(&format!(
        "linked {linked}; the gateway connects with it on its next start (prometheus -p {profile} gateway reload)"
    ))
}

/// What the bridge asks the user to do while pairing.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Shown<'a> {
    /// Scan this QR code's data (each refresh replaces the one before).
    Qr(&'a str),
    /// Type this pairing code on the phone.
    Code(&'a str),
}

/// The terminal's way of showing a pairing step.
fn show(shown: Shown<'_>) -> Result<(), Error> {
    match shown {
        Shown::Qr(data) => {
            let qr = qr_text(data)?;
            // Each refresh replaces the code shown before.
            println!(
                "\x1b[2J\x1b[H{qr}\nScan with WhatsApp: Settings > Linked devices > Link a device (refreshes until linked)"
            );
        }
        Shown::Code(code) => println!(
            "Pairing code: {code}\nOn the phone: WhatsApp > Linked devices > Link a device > Link with phone number instead"
        ),
    }
    Ok(())
}

/// The bridge command that links `scope` (replacing an earlier session
/// under `<profile>/platforms/whatsapp/session/`), by QR code or with a
/// pairing code for `phone`.
///
/// # Errors
///
/// `home.io` for the session directory, `config.invalid` for bad options.
pub(crate) fn link_command(
    scope: &pmt_config::ProfileScope,
    bridge: &Bridge,
    phone: Option<&str>,
) -> Result<tokio::process::Command, Error> {
    let session = scope
        .dir()
        .join("platforms")
        .join("whatsapp")
        .join("session");
    fresh_session(&session)?;
    let options: pmt_config::platforms::WhatsappOptions = pmt_config::platforms::options(
        "whatsapp",
        &scope.config().gateway.platform("whatsapp").options,
    )?;
    let mode = match options.mode {
        pmt_config::platforms::WhatsappMode::SelfChat => "self-chat",
        pmt_config::platforms::WhatsappMode::Bot => "bot",
    };
    let mut command = bridge.command();
    command
        .args(["--pair-only", "--pair-json"])
        .args(pmt_adapters_whatsapp::profile_args(scope.dir()))
        .args(["--mode", mode]);
    if let Some(phone) = phone {
        command.args(["--pairing-code", phone]);
    }
    Ok(command)
}

/// The terminal's width (`COLUMNS` when set, as shells export it).
fn columns() -> u16 {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse().ok())
        .or_else(|| crossterm::terminal::size().ok().map(|(c, _)| c))
        .unwrap_or(80)
}

/// How long pairing may take (shortened only in debug builds' tests).
pub(crate) fn pair_window() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(secs) = std::env::var("PMT_TEST_WHATSAPP_PAIR_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return Duration::from_secs(secs);
    }
    PAIR_WINDOW
}

/// How the bridge is started.
pub(crate) enum Bridge {
    /// The managed runtime: Node with the bridge script.
    Managed { node: PathBuf, script: PathBuf },
    /// A stand-in bridge executable (debug builds' tests).
    #[cfg(debug_assertions)]
    Fake(PathBuf),
}

impl Bridge {
    fn command(&self) -> tokio::process::Command {
        match self {
            Self::Managed { node, script } => {
                let mut command = tokio::process::Command::new(node);
                pmt_adapters_whatsapp::isolate(&mut command);
                command.arg(script);
                command
            }
            #[cfg(debug_assertions)]
            Self::Fake(exe) => tokio::process::Command::new(exe),
        }
    }
}

/// The bridge to pair with, installing the managed runtime when needed.
fn bridge(ctx: &Context<'_>) -> Result<Bridge, Error> {
    let root = ctx.home()?.root().to_path_buf();
    let progress: pmt_update::runtimes::install::Progress =
        std::sync::Arc::new(|line| eprintln!("{line}"));
    bridge_at(&root, &crate::update_cmd::outbound_policy(ctx), &progress)
}

/// The bridge of the home at `root`, installing the managed runtime (the
/// pinned Node, then the shipped bridge) through `policy` when needed.
///
/// # Errors
///
/// Runtime install errors.
pub(crate) fn bridge_at(
    root: &Path,
    policy: &pmt_net::http::OutboundPolicy,
    progress: &pmt_update::runtimes::install::Progress,
) -> Result<Bridge, Error> {
    #[cfg(debug_assertions)]
    if let Some(fake) = std::env::var_os("PMT_TEST_WHATSAPP_BRIDGE") {
        return Ok(Bridge::Fake(PathBuf::from(fake)));
    }
    let config = pmt_config::DaemonConfig::load(root)?;
    let store = pmt_update::runtimes::Store::open(root);
    let installer =
        crate::runtimes_cmd::browser::installer(store, &config, policy, Arc::clone(progress));
    let node = crate::model_cmd::block_on(installer.install("node"))??.exe();
    pmt_adapters_whatsapp::install::install(root, &node, &|line| progress(line))?;
    Ok(Bridge::Managed {
        node,
        script: pmt_adapters_whatsapp::install::dir(root).join("bridge.js"),
    })
}

/// Removes an earlier session and creates the directory owner-only.
fn fresh_session(dir: &Path) -> Result<(), Error> {
    let io = |e: &std::io::Error| Error::io(&codes::HOME_IO, dir, e);
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| io(&e))?;
    }
    std::fs::create_dir_all(dir).map_err(|e| io(&e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| io(&e))?;
    }
    Ok(())
}

/// The QR code of `data` as terminal text (two modules per character
/// row, with the quiet zone).
fn qr_text(data: &str) -> Result<String, Error> {
    let code = qrcode::QrCode::new(data.as_bytes()).map_err(|e| {
        Error::new(
            &codes::GATEWAY_DELIVERY_FAILED,
            format!("the bridge's QR data cannot be drawn: {e}"),
        )
    })?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .build())
}

/// Runs the pairing bridge until the phone links (the linked account),
/// the bridge fails, `window` passes or `cancel` fires (the bridge is
/// stopped); each step to show goes to `show`.
pub(crate) async fn pair(
    mut command: tokio::process::Command,
    window: Duration,
    cancel: &pmt_core::cancel::CancelToken,
    show: &mut (dyn FnMut(Shown<'_>) -> Result<(), Error> + Send),
) -> Result<String, Error> {
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            Error::new(
                &codes::RUNTIME_IO,
                format!("cannot start the WhatsApp bridge: {e}"),
            )
        })?;
    let stdout = child.stdout.take().expect("piped");
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let next = tokio::select! {
            () = cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(Error::new(&codes::GATEWAY_PAIRING_TIMEOUT, "WhatsApp pairing was cancelled"));
            }
            next = tokio::time::timeout_at(deadline, lines.next_line()) => next,
        };
        let line = match next {
            Err(_) => {
                let _ = child.kill().await;
                return Err(Error::new(
                    &codes::GATEWAY_PAIRING_TIMEOUT,
                    format!(
                        "WhatsApp was not linked within {} seconds; run the command again",
                        window.as_secs()
                    ),
                ));
            }
            Ok(Ok(Some(line))) => line,
            Ok(_) => {
                return Err(Error::new(
                    &codes::GATEWAY_DELIVERY_FAILED,
                    "the WhatsApp bridge stopped before the phone was linked",
                ));
            }
        };
        let event: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        if let Some(linked) = on_event(&event, show)? {
            // The bridge saves the credentials, then exits by itself.
            let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
            return Ok(linked);
        }
    }
}

/// Shows one pairing event; `Some(account)` once linked.
fn on_event(
    event: &Value,
    show: &mut (dyn FnMut(Shown<'_>) -> Result<(), Error> + Send),
) -> Result<Option<String>, Error> {
    match event["event"].as_str().unwrap_or_default() {
        "qr" => show(Shown::Qr(event["qr"].as_str().unwrap_or_default()))?,
        "pairing_code" => show(Shown::Code(event["code"].as_str().unwrap_or_default()))?,
        "connected" => {
            let user = event["user"]["name"]
                .as_str()
                .or_else(|| event["user"]["id"].as_str())
                .unwrap_or("WhatsApp");
            return Ok(Some(user.to_owned()));
        }
        "error" => {
            let why = event["error"].as_str().unwrap_or("unknown error");
            return Err(Error::new(
                &codes::GATEWAY_DELIVERY_FAILED,
                format!("WhatsApp pairing failed: {why}"),
            ));
        }
        _ => {}
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_qr_code_fits_sixty_columns() {
        let qr =
            qr_text("2@ZmFrZQ==,abcdefghijklmnopqrstuvwxyz0123456789,ABCDEFGHIJKLMNOP==,qrstuvwx")
                .unwrap();
        let widest = qr.lines().map(|l| l.chars().count()).max().unwrap();
        assert!(widest <= usize::from(MIN_COLUMNS), "{widest}");
    }

    #[test]
    fn events_link_fail_or_wait() {
        let mut shown = Vec::new();
        let mut show = |step: Shown<'_>| {
            shown.push(match step {
                Shown::Qr(data) | Shown::Code(data) => data.to_owned(),
            });
            Ok(())
        };
        assert_eq!(
            on_event(
                &json!({"event": "connected", "user": {"name": "Ana"}}),
                &mut show
            )
            .unwrap(),
            Some("Ana".to_owned())
        );
        assert!(on_event(&json!({"event": "error", "error": "logged_out"}), &mut show).is_err());
        assert_eq!(
            on_event(&json!({"event": "started"}), &mut show).unwrap(),
            None
        );
        assert_eq!(
            on_event(&json!({"event": "qr", "qr": "2@x"}), &mut show).unwrap(),
            None
        );
        assert_eq!(shown, ["2@x"]);
    }
}
