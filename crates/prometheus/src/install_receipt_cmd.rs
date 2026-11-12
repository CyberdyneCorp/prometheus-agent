//! `prometheus install-receipt --owner <manager>` (hidden; spec:
//! distribution-and-update, Package-manager and image channels): the one
//! way a package channel stamps the receipt beside the binary it
//! installed, so every channel writes the same, valid `install.json`.
//! The Homebrew formula runs it in `post_install`, the Dockerfile in a
//! build step, the Nix derivation in its install phase and the Termux
//! package in its `postinst`; `WinGet` cannot run scripts, so its location
//! implies the owner instead (`pmt_update::install::inferred_owner`).
//!
//! It runs before logging and home resolution: a package build has no
//! Prometheus home and must not grow one.

use std::path::PathBuf;

use clap::Args;
use pmt_core::error::{Error, codes};
use pmt_update::{Channel, InstallReceipt, Owner, ReleaseVersion};

use crate::build_info;

/// `install-receipt` arguments.
#[derive(Debug, Clone, Args)]
pub struct InstallReceiptArgs {
    /// Who updates this install (`homebrew`, `nix`, `termux`, `winget`,
    /// `image`, `system-package`, ...).
    #[arg(long, value_name = "OWNER")]
    pub owner: String,
    /// The release channel the package follows.
    #[arg(long, value_name = "CHANNEL", default_value = "stable")]
    pub channel: String,
    /// The installed binary (default: this executable).
    #[arg(long, value_name = "PATH")]
    pub binary: Option<PathBuf>,
}

/// Writes the receipt and prints its path.
///
/// # Errors
///
/// `cli.usage` (2) for an unknown or self-updating owner or channel;
/// `update.failed` (1) when the binary's directory is not writable.
pub fn run(args: &InstallReceiptArgs) -> Result<u8, Error> {
    let owner = Owner::parse(&args.owner)
        .filter(|o| !matches!(o, Owner::SelfInstall | Owner::Dev))
        .ok_or_else(|| {
            Error::new(
                &codes::CLI_USAGE,
                format!("--owner must name a package channel, not {}", args.owner),
            )
        })?;
    let channel: Channel = args.channel.parse()?;
    let binary = match &args.binary {
        Some(path) => std::fs::canonicalize(path),
        None => std::env::current_exe().and_then(std::fs::canonicalize),
    }
    .map_err(|e| {
        Error::new(
            &codes::UPDATE_FAILED,
            format!("cannot locate the binary: {e}"),
        )
    })?;
    let receipt = InstallReceipt {
        owner,
        channel,
        version: ReleaseVersion::parse(build_info::VERSION)?.to_string(),
        target: build_info::TARGET.to_owned(),
        installed_at: installed_at(),
        binary,
    };
    let path = receipt.write_beside()?;
    println!("{}", path.display());
    Ok(pmt_core::exit::SUCCESS)
}

/// `SOURCE_DATE_EPOCH` when a reproducible build sets it (Nix), else now.
fn installed_at() -> String {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|secs| jiff::Timestamp::from_second(secs).ok())
        .unwrap_or_else(jiff::Timestamp::now)
        .to_string()
}
