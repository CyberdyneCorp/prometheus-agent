//! `prometheus bundle create --target <t> [--version v] [--with
//! node,cpython,chromium] -o <file>` (spec: distribution-and-update,
//! Offline bundles): downloads and verifies one release for a target,
//! optionally with runtime archives, into a single file that
//! `install.sh --bundle`, `install.ps1 -Bundle`, `prometheus update
//! --from-bundle` and `prometheus runtimes install --from-bundle` install
//! with no network access.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use pmt_config::daemon::DaemonConfig;
use pmt_core::error::Error;
use pmt_update::bundle::{self, Request};
use pmt_update::{Channel, Identity, ReleaseVersion, Releases, TrustRoot};

use crate::build_info;
use crate::context::Context;

/// `prometheus bundle` subcommands.
#[derive(Debug, Subcommand)]
pub enum BundleCommand {
    /// Write an offline bundle for a target.
    Create(CreateArgs),
}

/// `bundle create` arguments.
#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Target triple of the binary (for example
    /// `x86_64-unknown-linux-musl`, `universal2-apple-darwin`).
    #[arg(long, value_name = "TARGET")]
    pub target: String,
    /// Release tag (default: the channel's newest).
    #[arg(long, value_name = "VERSION")]
    pub version: Option<String>,
    /// Channel read when `--version` is not given.
    #[arg(long, value_name = "CHANNEL", default_value = "stable")]
    pub channel: String,
    /// Runtimes to include, comma separated (node, cpython, uv, chromium).
    #[arg(long = "with", value_name = "RUNTIMES", value_delimiter = ',')]
    pub runtimes: Vec<String>,
    /// Output file.
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub output: PathBuf,
}

/// Runs `prometheus bundle`.
///
/// # Errors
///
/// `update.invalid_argument` (2), `update.failed` (1), `update.signature`
/// (77), `runtime.unknown` (2).
pub fn run(command: &BundleCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let BundleCommand::Create(args) = command;
    let version = args
        .version
        .as_deref()
        .map(ReleaseVersion::parse)
        .transpose()?;
    let channel: Channel = args.channel.parse()?;
    let index = pmt_update::runtimes::index::RuntimeIndex::embedded();
    for name in &args.runtimes {
        index.pin(name)?;
    }
    let config = DaemonConfig::load(ctx.home()?.root())?.update;
    let releases = Releases::new(
        config.mirror_url.as_deref(),
        Identity {
            version: build_info::VERSION,
            target: build_info::TARGET,
        },
        &crate::update_cmd::outbound_policy(ctx),
        TrustRoot::embedded(),
        config.require_transparency_log,
    )?
    .with_fallback(config.mirror_fallback);
    let request = Request {
        version,
        channel,
        target: args.target.clone(),
        runtimes: args.runtimes.clone(),
    };
    let progress = |line: &str| eprintln!("{line}");
    let made =
        crate::model_cmd::block_on(bundle::create(&releases, &request, &args.output, &progress))??;
    println!(
        "wrote {} ({} for {}{})",
        args.output.display(),
        made.version,
        made.target,
        if made.runtimes.is_empty() {
            String::new()
        } else {
            format!(", with {}", made.runtimes.join(", "))
        }
    );
    Ok(0)
}
