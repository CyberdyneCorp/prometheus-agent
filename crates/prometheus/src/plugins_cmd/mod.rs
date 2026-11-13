//! `prometheus plugins ...` (spec: plugin-system, plugins CLI; Opt-in
//! enablement; Capability grants are user-approved and digest-bound;
//! Plugin catalog; Catalog kill list; Signed plugin packages and
//! provenance; Hot reload respects the prompt-cache invariant;
//! platform-adapters, Adapter conformance kit).
//!
//! `dev` (M3), `run` (plugin CLI subcommands), `collection` (M3) and
//! `hooks` (shell hooks) arrive with their owners.

mod adapter;
#[cfg(feature = "gateway")]
mod adapter_plugin;
mod catalog_cmd;
pub mod hooks_cmd;
mod inspect;
pub(crate) mod install;
pub(crate) mod manage;
mod package_cmd;
mod python;
mod reload;
mod run_bridged;
pub mod run_cmd;
mod threat;
mod update;

use std::path::PathBuf;

use clap::{Args, Subcommand};
use pmt_core::error::Error;

use crate::context::Context;

/// `prometheus plugins` subcommands.
#[derive(Debug, Subcommand)]
pub enum PluginsCommand {
    /// Install a plugin: a catalog id (its pinned ref), a `.pmtpkg` file,
    /// or a Git URL with `--ref <full commit SHA>`.
    Install(InstallArgs),
    /// Catalog entries matching a word.
    Search {
        /// Matched against id, category, description and capabilities.
        query: String,
    },
    /// Every catalog entry, by tier and category.
    Browse,
    /// A plugin's details: installed state, worlds, capabilities and
    /// provenance, and its catalog entry.
    Show {
        /// The plugin id.
        id: String,
    },
    /// Every discovered plugin: id, version, kind, source, state
    /// (`loaded`, `disabled`, `unadopted` or `error`) and why it does not
    /// load.
    List,
    /// Enable a plugin: shows its capabilities and asks you to approve
    /// them (non-interactively, `--grant all` or `--grant <cap>,...`
    /// approves; otherwise exits 4). It loads in sessions started afterwards.
    Enable {
        /// The plugin id.
        id: String,
        /// Approve `all` requested capabilities, or a comma list
        /// (`network api.example.com`, `secrets.read:TOKEN`).
        #[arg(long, value_name = "all|CAPABILITIES")]
        grant: Option<String>,
    },
    /// Disable a plugin (it keeps its grants).
    Disable {
        /// The plugin id.
        id: String,
    },
    /// Update a plugin, or every plugin whose catalog ref or package moved;
    /// one asking for new capabilities keeps its current version active
    /// until they are approved.
    Update {
        /// The plugin id (every plugin when omitted).
        id: Option<String>,
        /// Approve the new capabilities (`all` or a comma list).
        #[arg(long, value_name = "all|CAPABILITIES")]
        grant: Option<String>,
        /// Accept a package without a verified signature (recorded).
        #[arg(long)]
        allow_unsigned: bool,
        /// Update although the threat scan graded the new version
        /// `caution`; a `dangerous` one is refused and the installed
        /// version stays active.
        #[arg(long)]
        force: bool,
    },
    /// List the plugins whose catalog ref or package moved.
    CheckUpdates,
    /// Runs a command a plugin registered (`cli_commands`), or lists them.
    Run {
        /// The plugin id.
        id: String,
        /// The command (its list when omitted).
        command: Option<String>,
        /// Its arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Shell hooks, gateway hook directories and outbound webhooks.
    Hooks {
        /// What to do.
        #[command(subcommand)]
        action: hooks_cmd::HooksAction,
    },
    /// The capabilities a staged update asks for beyond the current grant.
    DiffGrants {
        /// The plugin id.
        id: String,
    },
    /// Remove an installed plugin (its grants are revoked; its data in
    /// `plugin-data/` is kept).
    Remove {
        /// The plugin id.
        id: String,
    },
    /// Show, approve or revoke a plugin's capability grants.
    Grants {
        /// The grants action.
        #[command(subcommand)]
        command: GrantsAction,
    },
    /// Declared versus granted capabilities, per plugin.
    Capabilities {
        /// One plugin (every plugin when omitted).
        id: Option<String>,
    },
    /// Register a plugin directory placed by hand in the profile's
    /// `plugins/`: validate it, record it as `adopted` and approve its
    /// capabilities.
    Adopt {
        /// The directory (or the plugin id).
        dir: String,
        /// Approve `all` requested capabilities, or a comma list.
        #[arg(long, value_name = "all|CAPABILITIES")]
        grant: Option<String>,
    },
    /// Check a plugin directory: manifest, WIT world compatibility and
    /// capability declarations (exit 65 on any error).
    Validate {
        /// The plugin directory.
        path: PathBuf,
    },
    /// Pack a plugin directory into a `.pmtpkg` package.
    Pack {
        /// The plugin directory.
        dir: PathBuf,
        /// Where to write it (default: `<id>-<version>.pmtpkg` here).
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    /// Sign a package's digest with a minisign secret key: writes
    /// `<package>.minisig`.
    Sign {
        /// The `.pmtpkg` file.
        package: PathBuf,
        /// An unencrypted minisign secret key file.
        #[arg(long, value_name = "FILE")]
        key: PathBuf,
    },
    /// Problems of the profile's plugins: load errors, unsigned or
    /// unadopted plugins, plugins modified after install or removed from
    /// the catalog.
    Doctor,
    /// Replace the profile's plugin instances (in the running daemon when
    /// there is one): hooks, slash commands and event subscribers change
    /// at once, tools and system-prompt sections in sessions started
    /// afterwards (`/plugins reload --now` applies them to a session).
    Reload {
        /// The plugin to report on (every plugin reloads).
        id: Option<String>,
    },
    /// Run the adapter conformance kit against a mock of the platform:
    /// a first-party platform name (`telegram`, `discord`, `slack`,
    /// `email`, `matrix`, `signal`, `whatsapp`, `webhook`; builds with the
    /// `adapter-kits` feature) or a `platform-adapter` plugin directory.
    TestAdapter {
        /// The platform name or plugin directory.
        path: String,
    },
}

/// `prometheus plugins install` options.
#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)] // independent command-line switches
pub struct InstallArgs {
    /// A catalog id, a `.pmtpkg` path or a Git URL.
    pub identifier: String,
    /// The full commit SHA of a Git install.
    #[arg(long = "ref", value_name = "SHA")]
    pub git_ref: Option<String>,
    /// Install without a verified signature (recorded on the install
    /// record).
    #[arg(long)]
    pub allow_unsigned: bool,
    /// Install although the catalog removed the plugin (recorded).
    #[arg(long)]
    pub allow_removed: bool,
    /// Enable it now (asks to approve its capabilities).
    #[arg(long, conflicts_with = "no_enable")]
    pub enable: bool,
    /// Leave it disabled.
    #[arg(long)]
    pub no_enable: bool,
    /// With `--enable`: approve `all` requested capabilities, or a list.
    #[arg(long, value_name = "all|CAPABILITIES", requires = "enable")]
    pub grant: Option<String>,
    /// Install although the threat scan graded it `caution` (recorded); a
    /// `dangerous` verdict is refused anyway.
    #[arg(long)]
    pub force: bool,
}

/// `prometheus plugins grants` actions.
#[derive(Debug, Subcommand)]
pub enum GrantsAction {
    /// Requested capabilities, in plain language, and whether each is granted.
    Show {
        /// The plugin id.
        id: String,
    },
    /// Approve every requested capability, or one, for the plugin as it is
    /// now (its content digest).
    Approve {
        /// The plugin id.
        id: String,
        /// One capability (`secrets GITHUB_TOKEN` or `secrets.read:GITHUB_TOKEN`).
        capability: Option<String>,
    },
    /// Revoke the plugin's grants, or one; a running plugin is refused it
    /// from its next call.
    Revoke {
        /// The plugin id.
        id: String,
        /// One capability.
        capability: Option<String>,
    },
}

/// Runs one `plugins` subcommand.
///
/// # Errors
///
/// The subcommand's failure; the caller prints it and exits with its code.
pub fn run(command: &PluginsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let code = dispatch(command, ctx)?;
    let target = match changed(command) {
        Change::None => return Ok(code),
        Change::Plugin(id) => Some(id),
        Change::Plugins => None,
    };
    if code == 0 {
        reload::after_change(ctx, target);
    }
    Ok(code)
}

/// What a command changes of which plugins load.
enum Change<'a> {
    None,
    /// One plugin, which stays installed (the reload reports on it).
    Plugin(&'a str),
    /// Some plugins.
    Plugins,
}

fn changed(command: &PluginsCommand) -> Change<'_> {
    match command {
        PluginsCommand::Enable { id, .. }
        | PluginsCommand::Disable { id }
        | PluginsCommand::Update { id: Some(id), .. }
        | PluginsCommand::Grants {
            command: GrantsAction::Approve { id, .. } | GrantsAction::Revoke { id, .. },
        } => Change::Plugin(id),
        PluginsCommand::Install(_)
        | PluginsCommand::Update { id: None, .. }
        | PluginsCommand::Remove { .. }
        | PluginsCommand::Adopt { .. }
        | PluginsCommand::Hooks {
            action: hooks_cmd::HooksAction::Allow { .. } | hooks_cmd::HooksAction::Revoke { .. },
        } => Change::Plugins,
        _ => Change::None,
    }
}

fn dispatch(command: &PluginsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        PluginsCommand::Install(args) => crate::security_cmd::audit_signature(
            ctx,
            &format!("plugin {}", args.identifier),
            install::install(ctx, args),
        ),
        PluginsCommand::Search { query } => catalog_cmd::search(ctx, Some(query)),
        PluginsCommand::Browse => catalog_cmd::search(ctx, None),
        PluginsCommand::Show { id } => catalog_cmd::show(ctx, id),
        PluginsCommand::List => manage::list(ctx),
        PluginsCommand::Enable { id, grant } => manage::enable(ctx, id, grant.as_deref()),
        PluginsCommand::Disable { id } => manage::disable(ctx, id),
        PluginsCommand::Update {
            id,
            grant,
            allow_unsigned,
            force,
        } => update::update(
            ctx,
            id.as_deref(),
            grant.as_deref(),
            update::Flags {
                allow_unsigned: *allow_unsigned,
                force: *force,
            },
        ),
        PluginsCommand::CheckUpdates => update::check(ctx),
        PluginsCommand::Hooks { action } => hooks_cmd::run(ctx, action),
        PluginsCommand::Run { id, command, args } => {
            run_cmd::run(ctx, id, command.as_deref(), args)
        }
        PluginsCommand::DiffGrants { id } => update::diff_grants(ctx, id),
        PluginsCommand::Remove { id } => inspect::remove(ctx, id),
        PluginsCommand::Grants { command } => manage::grants(ctx, command),
        PluginsCommand::Capabilities { id } => inspect::capabilities(ctx, id.as_deref()),
        PluginsCommand::Adopt { dir, grant } => inspect::adopt(ctx, dir, grant.as_deref()),
        PluginsCommand::Validate { path } => inspect::validate(path),
        PluginsCommand::Pack { dir, out } => package_cmd::pack(dir, out.as_deref()),
        PluginsCommand::Sign { package, key } => package_cmd::sign(package, key),
        PluginsCommand::Doctor => inspect::doctor(ctx),
        PluginsCommand::Reload { id } => reload::reload(ctx, id.as_deref()),
        PluginsCommand::TestAdapter { path } => adapter::test_adapter(path),
    }
}
