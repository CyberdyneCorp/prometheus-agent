//! `prometheus gateway telegram logout` (spec: platform-adapters, Telegram
//! local Bot API server): a bot moving to a self-hosted Bot API server is
//! logged out of the public one once (`logOut`), with the profile's token.

use clap::{Args, Subcommand};
use pmt_core::error::{Error, codes};

use crate::context::Context;

/// `prometheus gateway telegram`.
#[derive(Debug, Clone, Args)]
pub struct TelegramArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub command: TelegramCommand,
}

/// `gateway telegram` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum TelegramCommand {
    /// Log the bot out of the public Bot API server, once, before
    /// `options.api_base_url` points it at a self-hosted server (Telegram
    /// then refuses the bot on the public server for 10 minutes).
    Logout,
}

/// Runs `gateway telegram <command>`.
///
/// # Errors
///
/// The token is missing, or Telegram refused the call.
pub(super) fn run(args: &TelegramArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let TelegramCommand::Logout = args.command;
    let scope = ctx.scope()?;
    let table = scope
        .config()
        .gateway
        .platform(pmt_adapters_telegram::PLATFORM);
    let secret = table
        .token_secret
        .map_or_else(|| pmt_adapters_telegram::TOKEN_SECRET.to_owned(), |s| s.0);
    let token = scope.secrets().resolve(&secret)?;
    let endpoints = public_endpoints();
    let client = pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())?
        .build()
        .map_err(|e| Error::new(&codes::NET_CLIENT_INIT, e.to_string()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot start the runtime: {e}")))?;
    runtime
        .block_on(pmt_adapters_telegram::log_out(
            &client,
            &endpoints,
            token.expose(),
        ))
        .map_err(|e| {
            Error::new(
                &codes::GATEWAY_DELIVERY_FAILED,
                format!("logOut: {}", e.describe()),
            )
        })?;
    crate::output::print_raw(
        "logged the bot out of the public Bot API server; set gateway.platforms.telegram.options.api_base_url to the self-hosted server\n",
    )
}

/// The public Bot API (a stand-in in debug builds' tests).
fn public_endpoints() -> pmt_adapters_telegram::Endpoints {
    #[cfg(debug_assertions)]
    if let Some(api) = std::env::var("PMT_TEST_TELEGRAM_API")
        .ok()
        .filter(|a| !a.is_empty())
    {
        return pmt_adapters_telegram::Endpoints::at(&api);
    }
    pmt_adapters_telegram::Endpoints::default()
}
