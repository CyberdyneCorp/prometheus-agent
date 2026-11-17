//! Arguments of the `profile` subcommands that carry more than a name
//! (kept here so `cli.rs` stays a table of commands).

use clap::{Args, Subcommand};

/// `profile describe`.
#[derive(Debug, Args)]
pub struct DescribeArgs {
    /// Profile name (default: the selected one; ignored with --all).
    pub name: Option<String>,
    /// The description (at most 500 characters).
    #[arg(long, value_name = "TEXT", conflicts_with = "auto")]
    pub text: Option<String>,
    /// Write one with the auxiliary model from the profile's name,
    /// identity file, model and skills.
    #[arg(long)]
    pub auto: bool,
    /// Replace an existing description.
    #[arg(long)]
    pub overwrite: bool,
    /// Describe every profile that has no description yet.
    #[arg(long)]
    pub all: bool,
}

/// `profile install`.
#[derive(Debug, Args)]
pub struct InstallArgs {
    /// A git URL or a directory holding `distribution.toml`.
    pub source: String,
    /// The new profile's name (default: the manifest's `name`).
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Also write the alias launcher `<name>`.
    #[arg(long)]
    pub alias: bool,
    /// Install over an existing profile of that name.
    #[arg(long)]
    pub force: bool,
    /// Do not ask for confirmation.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// `profile update`.
#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// The profile a distribution was installed into.
    pub name: String,
    /// Also replace `config.toml` with the distribution's.
    #[arg(long)]
    pub force_config: bool,
    /// Do not ask for confirmation.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// `profile identity`.
#[derive(Debug, Subcommand)]
pub enum IdentityCommand {
    /// Copy a profile's identity file and identity memory to another
    /// profile (applies from its next session).
    Move {
        /// The profile to copy from.
        from: String,
        /// The profile to copy to.
        to: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Remove a profile's identity file and identity memory, after a
    /// snapshot (applies from its next session).
    Purge {
        /// Profile name.
        name: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}
