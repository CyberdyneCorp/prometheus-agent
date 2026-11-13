//! `prometheus desktop` (alias `gui`; spec: desktop-app, Launching the
//! desktop from the CLI): launches the installed desktop app for the
//! selected profile, opening a draft through the `prometheus://new` deep
//! link (with `--cwd` as its workspace); `--install` downloads the app
//! bundle of this binary's version and channel and installs it only after
//! its signature verifies against the embedded release keys;
//! `--ignore-existing` starts the app without ensuring a local daemon.

mod catalog;
mod install;
mod locate;

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};

use crate::context::Context;
use crate::output::print_raw;

/// `prometheus desktop` arguments.
#[derive(Debug, Clone, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct DesktopArgs {
    /// The initial workspace of new chats.
    #[arg(long, value_name = "PATH")]
    pub cwd: Option<PathBuf>,
    /// Download and install the desktop app of this version and channel.
    #[arg(long, conflicts_with_all = ["cwd", "ignore_existing"])]
    pub install: bool,
    /// Start the app without ensuring a local daemon (to pick a remote
    /// connection).
    #[arg(long)]
    pub ignore_existing: bool,
    /// Internal subcommands of the desktop shell.
    #[command(subcommand)]
    pub command: Option<DesktopCommand>,
}

/// What the desktop shell asks its bundled binary.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum DesktopCommand {
    /// The install dialog's facts about a catalog entry, as JSON.
    #[command(hide = true)]
    CatalogEntry {
        /// `plugin` or `skill`.
        kind: catalog::Kind,
        /// The catalog id.
        id: String,
    },
}

/// The message of a missing app.
pub const NOT_INSTALLED: &str =
    "Prometheus Desktop is not installed; run 'prometheus desktop --install'";

/// Runs `prometheus desktop`.
///
/// # Errors
///
/// `desktop.not_installed` (69); download and signature errors of
/// `--install` (`update.signature`, 77).
pub fn run(args: &DesktopArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    if let Some(DesktopCommand::CatalogEntry { kind, id }) = &args.command {
        return catalog::entry(ctx, *kind, id);
    }
    let user_home = ctx
        .launch
        .user_home()
        .cloned()
        .ok_or_else(|| Error::new(&codes::DESKTOP_NOT_INSTALLED, NOT_INSTALLED))?;
    if args.install {
        return install::install(ctx, &user_home);
    }
    let app = locate::installed(&user_home, ctx.launch)
        .ok_or_else(|| Error::new(&codes::DESKTOP_NOT_INSTALLED, NOT_INSTALLED))?;
    let profile = ctx.home()?.select_profile(ctx.profile)?.to_string();
    let link = new_chat_link(&profile, args.cwd.as_deref())?;
    let mut command = std::process::Command::new(&app);
    command.arg(&link);
    if args.ignore_existing {
        command.arg("--ignore-existing");
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command.spawn().map_err(|e| {
        Error::new(
            &codes::DESKTOP_NOT_INSTALLED,
            format!("{} could not be started: {e}", app.display()),
        )
    })?;
    print_raw(&format!("Opened Prometheus Desktop ({profile})\n"))
}

/// `prometheus://new?profile=<p>[&cwd=<absolute path>]`.
fn new_chat_link(profile: &str, cwd: Option<&Path>) -> Result<String, Error> {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("profile", profile);
    if let Some(cwd) = cwd {
        let absolute =
            std::path::absolute(cwd).map_err(|e| Error::io(&codes::CLI_USAGE, cwd, &e))?;
        if !absolute.is_dir() {
            return Err(Error::new(
                &codes::CLI_USAGE,
                format!("--cwd {} is not a directory", absolute.display()),
            ));
        }
        query.append_pair("cwd", &absolute.to_string_lossy());
    }
    Ok(format!("prometheus://new?{}", query.finish()))
}
