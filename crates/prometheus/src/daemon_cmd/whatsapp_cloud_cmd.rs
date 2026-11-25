//! `prometheus gateway whatsapp-cloud setup` (spec: platform-adapters,
//! `WhatsApp` Cloud API adapter): generates the webhook verify token Meta's
//! subscription handshake must echo, stores it in the secrets store under
//! the name `[gateway.platforms.whatsapp_cloud] options.verify_token_secret`
//! gives (default `WHATSAPP_CLOUD_VERIFY_TOKEN`), and prints it with the
//! callback route to paste into the Meta app's webhook settings. Running
//! it again replaces the token.

use std::fmt::Write as _;

use clap::{Args, Subcommand};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;

use crate::context::Context;

const PLATFORM: &str = "whatsapp_cloud";
/// The verify token's secret unless the options name another.
const DEFAULT_VERIFY_SECRET: &str = "WHATSAPP_CLOUD_VERIFY_TOKEN";

/// `prometheus gateway whatsapp-cloud`.
#[derive(Debug, Clone, Args)]
pub struct WhatsappCloudArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub command: WhatsappCloudCommand,
}

/// `gateway whatsapp-cloud` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum WhatsappCloudCommand {
    /// Generate the webhook verify token (replacing an earlier one), store
    /// it and print it with the callback route.
    Setup,
}

/// A fresh verify token: 32 random bytes, hex.
fn fresh_token() -> Result<String, Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| {
        Error::new(
            &codes::HOME_IO,
            format!("the OS random generator failed: {e}"),
        )
    })?;
    Ok(bytes.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    }))
}

/// Runs `gateway whatsapp-cloud ...`.
///
/// # Errors
///
/// Profile and secrets store errors.
pub(super) fn run(args: &WhatsappCloudArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let WhatsappCloudCommand::Setup = args.command;
    let scope = ctx.scope()?;
    let table = scope.config().gateway.platform(PLATFORM);
    let name = table
        .options
        .get("verify_token_secret")
        .and_then(|v| v.as_str())
        .filter(|n| !n.is_empty())
        .unwrap_or(DEFAULT_VERIFY_SECRET)
        .to_owned();
    let token = fresh_token()?;
    scope
        .secrets()
        .store()
        .set(&name, &SecretString::new(&token))?;
    let listen = pmt_config::DaemonConfig::load(scope.home())
        .map(|c| c.gateway.http.listen)
        .unwrap_or_default();
    let mut text = String::new();
    let _ = writeln!(text, "Verify token (stored as secret {name}): {token}");
    let _ = writeln!(
        text,
        "Callback route: http://{listen}/p/{}/{PLATFORM}/ (publish it over HTTPS)",
        scope.name()
    );
    let _ = writeln!(
        text,
        "Paste the public callback URL and the token into the Meta app's WhatsApp webhook settings."
    );
    crate::output::print_raw(&text)?;
    Ok(0)
}
