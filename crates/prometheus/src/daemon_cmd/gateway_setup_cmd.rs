//! `prometheus gateway setup <platform>` (spec: platform-adapters, Feishu
//! and Lark adapter; Weixin personal WeChat adapter): the platforms whose
//! account is created or logged in by scanning a QR code with the
//! platform's app. Feishu creates the bot app ([`super::feishu_cmd`]),
//! Weixin logs a personal account in ([`super::weixin_cmd`]); each stores
//! its credential in the secrets store and its ids in the platform
//! options. Other platforms are set up with `prometheus setup gateway`.

use clap::{Args, ValueEnum};
use pmt_core::error::{Error, codes};

use crate::context::Context;

/// The platforms set up by QR scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum QrPlatform {
    /// Feishu or Lark: create the bot app.
    Feishu,
    /// Weixin (personal WeChat): log the account in.
    Weixin,
}

/// `prometheus gateway setup`.
#[derive(Debug, Clone, Args)]
pub struct GatewaySetupArgs {
    /// The platform.
    pub platform: QrPlatform,
    /// Feishu only: create the app on `feishu` or `lark` (a Lark account
    /// scanning a Feishu code is detected and switches it).
    #[arg(long, value_parser = ["feishu", "lark"])]
    pub domain: Option<String>,
}

/// Runs `gateway setup <platform>`.
///
/// # Errors
///
/// The platform's setup failed (see its module).
pub(super) fn run(args: &GatewaySetupArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot start the runtime: {e}")))?;
    let text = match args.platform {
        QrPlatform::Feishu => {
            let configured = scope
                .config()
                .gateway
                .platforms
                .get("feishu")
                .and_then(|p| {
                    p.options
                        .get("domain")
                        .and_then(|d| d.as_str().map(str::to_owned))
                });
            let domain = args
                .domain
                .clone()
                .or(configured)
                .unwrap_or_else(|| "feishu".to_owned());
            runtime.block_on(super::feishu_cmd::setup(&scope, &domain))?
        }
        QrPlatform::Weixin => runtime.block_on(super::weixin_cmd::setup(&scope))?,
    };
    crate::output::print_raw(&text)?;
    Ok(0)
}
