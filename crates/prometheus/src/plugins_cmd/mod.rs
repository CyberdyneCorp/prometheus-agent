//! `prometheus plugins ...` (spec: plugin-system, plugins CLI; Opt-in
//! enablement; Capability grants are user-approved and digest-bound;
//! platform-adapters, Adapter conformance kit).

mod adapter;
mod manage;

use clap::Subcommand;
use pmt_core::error::Error;

use crate::context::Context;

/// `prometheus plugins` subcommands.
#[derive(Debug, Subcommand)]
pub enum PluginsCommand {
    /// Every discovered plugin: id, version, kind, source, state
    /// (`loaded`, `disabled` or `error`) and why it does not load.
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
    /// Show, approve or revoke a plugin's capability grants.
    Grants {
        /// The grants action.
        #[command(subcommand)]
        command: GrantsAction,
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
    match command {
        PluginsCommand::List => manage::list(ctx),
        PluginsCommand::Enable { id, grant } => manage::enable(ctx, id, grant.as_deref()),
        PluginsCommand::Disable { id } => manage::disable(ctx, id),
        PluginsCommand::Grants { command } => manage::grants(ctx, command),
        PluginsCommand::TestAdapter { path } => adapter::test_adapter(path),
    }
}
