//! `prometheus vault` arguments (spec: security-boundaries, Vault
//! management CLI).

use clap::{Args, Subcommand, ValueEnum};

/// `prometheus vault` subcommands.
#[derive(Debug, Subcommand)]
pub enum VaultCommand {
    /// List the vault's items: handle, kind, label, origins and identifier
    /// (never a secret value).
    List,
    /// Add a login, card or address; secret values are asked with a masked
    /// prompt (or read from stdin, one per line), never taken from the
    /// command line.
    Add(Box<AddArgs>),
    /// Delete an item after confirmation.
    Rm {
        /// The item's handle (`prometheus vault list`).
        handle: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Show password-manager login sources, or enable or disable one.
    Sources {
        /// Enable a source.
        #[arg(long, value_enum, conflicts_with = "disable")]
        enable: Option<SourceArg>,
        /// Disable a source.
        #[arg(long, value_enum)]
        disable: Option<SourceArg>,
    },
    /// Show where the vault key lives and whether a keychain is reachable.
    Key {
        /// The key subcommand.
        #[command(subcommand)]
        command: Option<KeyCommand>,
    },
}

/// `prometheus vault key` subcommands.
#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Move the key to the keychain or the file: written there, checked
    /// against the vault, and only then removed from where it was.
    Migrate {
        /// Where the key goes.
        #[arg(long, value_enum)]
        to: KeyTarget,
    },
}

/// `vault key migrate --to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KeyTarget {
    /// The OS keychain.
    Keychain,
    /// `<profile>/vault/vault.key`.
    File,
}

/// A password-manager source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SourceArg {
    /// 1Password (`op`).
    Onepassword,
    /// Bitwarden (`bw`).
    Bitwarden,
}

/// `prometheus vault add`.
#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)] // the item kind flags and their refusals
pub struct AddArgs {
    /// Add a login (default).
    #[arg(long, conflicts_with_all = ["card", "address"])]
    pub login: bool,
    /// Add a payment card.
    #[arg(long, conflicts_with = "address")]
    pub card: bool,
    /// Add a postal address.
    #[arg(long)]
    pub address: bool,
    /// A name for the item.
    #[arg(long)]
    pub label: Option<String>,
    /// A login's site, `https://host` (repeatable).
    #[arg(long = "origin")]
    pub origins: Vec<String>,
    /// A login's user name or email.
    #[arg(long)]
    pub identifier: Option<String>,
    /// Ask for a TOTP seed too (a login).
    #[arg(long)]
    pub with_totp: bool,
    /// A card's holder name.
    #[arg(long)]
    pub holder: Option<String>,
    /// A card's expiry, `MM/YY`.
    #[arg(long)]
    pub expiry: Option<String>,
    /// An address's full name.
    #[arg(long)]
    pub name: Option<String>,
    /// An address's street line.
    #[arg(long)]
    pub line1: Option<String>,
    /// An address's second line.
    #[arg(long)]
    pub line2: Option<String>,
    /// An address's city.
    #[arg(long)]
    pub city: Option<String>,
    /// An address's state or region.
    #[arg(long)]
    pub region: Option<String>,
    /// An address's postal code.
    #[arg(long)]
    pub postal_code: Option<String>,
    /// An address's country.
    #[arg(long)]
    pub country: Option<String>,
    /// Refused: secrets are read only from a masked prompt.
    #[arg(long, hide = true, allow_hyphen_values = true)]
    pub password: Option<String>,
    /// Refused: secrets are read only from a masked prompt.
    #[arg(long, hide = true, allow_hyphen_values = true)]
    pub totp: Option<String>,
    /// Refused: secrets are read only from a masked prompt.
    #[arg(long, hide = true, allow_hyphen_values = true)]
    pub number: Option<String>,
    /// Refused: secrets are read only from a masked prompt.
    #[arg(long, hide = true, allow_hyphen_values = true)]
    pub cvc: Option<String>,
}

impl AddArgs {
    /// The secret-bearing flag given on the command line, if any.
    #[must_use]
    pub fn secret_flag(&self) -> Option<&'static str> {
        [
            ("--password", self.password.is_some()),
            ("--totp", self.totp.is_some()),
            ("--number", self.number.is_some()),
            ("--cvc", self.cvc.is_some()),
        ]
        .into_iter()
        .find_map(|(flag, given)| given.then_some(flag))
    }
}
