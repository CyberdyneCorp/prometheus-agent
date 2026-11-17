//! `prometheus secrets bitwarden|onepassword` subcommands.

use clap::Subcommand;

/// `prometheus secrets bitwarden`.
#[derive(Debug, Subcommand)]
pub enum BitwardenCommand {
    /// Install `bws` if needed, store and check the access token, select
    /// the project and turn the source on.
    Setup {
        /// The project whose secrets are read (default: every project the
        /// token sees).
        #[arg(long, value_name = "ID")]
        project_id: Option<String>,
        /// A self-hosted server's URL.
        #[arg(long, value_name = "URL")]
        server_url: Option<String>,
        /// Install the CLI without asking.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Show the CLI version, whether the token works and the names the
    /// source supplies (never values).
    Status,
    /// Replace the access token after checking the new one.
    Token,
    /// Preview the names the project would add or that other sources
    /// shadow; `--apply` accepts that list.
    Sync {
        /// Record the previewed names as the ones the source supplies.
        #[arg(long)]
        apply: bool,
    },
    /// Install the pinned `bws` into the runtime store.
    Install {
        /// Do not ask for confirmation.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Turn the source off (`bws` is then never run).
    Disable,
}

/// `prometheus secrets onepassword`.
#[derive(Debug, Subcommand)]
pub enum OnePasswordCommand {
    /// Install `op` if needed, store and check the service-account token,
    /// select the vault and turn the source on.
    Setup {
        /// The vault `sync` reads.
        #[arg(long, value_name = "VAULT")]
        vault: Option<String>,
        /// Install the CLI without asking.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Show the CLI version, whether the token works and the mapped names
    /// (never values).
    Status,
    /// Map a secret name to an `op://vault/item/field` reference.
    Map {
        /// The secret name.
        name: String,
        /// Its `op://` reference.
        reference: String,
    },
    /// Remove a mapping.
    Unmap {
        /// The secret name.
        name: String,
    },
    /// Preview mappings for the vault's items named like secrets;
    /// `--apply` writes them.
    Sync {
        /// Write the previewed mappings.
        #[arg(long)]
        apply: bool,
    },
    /// Turn the source off (`op` is then never run).
    Disable,
}
