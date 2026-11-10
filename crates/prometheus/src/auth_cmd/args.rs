//! `prometheus auth` arguments (spec: model-providers).

use std::path::PathBuf;

use clap::Subcommand;

/// `prometheus auth` subcommands.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store an API key (read from a hidden prompt or stdin) or sign in to
    /// a subscription.
    Add {
        /// Provider id or alias.
        provider: String,
        /// Credential type (default: `oauth` for sign-in-only providers,
        /// else `api_key`).
        #[arg(long = "type", value_enum, value_name = "TYPE")]
        kind: Option<AuthTypeArg>,
        /// Label of this credential (default `default`).
        #[arg(long, value_name = "LABEL")]
        label: Option<String>,
        /// Sign in through the browser (PKCE on a loopback redirect).
        #[arg(long, conflicts_with = "device")]
        browser: bool,
        /// Sign in with a device code.
        #[arg(long)]
        device: bool,
        /// Put the new API key at this 0-based position of the pool order.
        #[arg(long, value_name = "N")]
        priority: Option<usize>,
        /// Refused: keys are never accepted as arguments.
        #[arg(hide = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv_key: Vec<String>,
    },
    /// List every credential: provider, label, kind, source and status
    /// (never key or token material).
    List,
    /// One provider's credentials, sign-in flows and plan notes.
    Status {
        /// Provider id or alias.
        provider: String,
    },
    /// Delete a credential (and revoke a sign-in's refresh token where the
    /// provider offers revocation).
    #[command(visible_alias = "rm")]
    Remove {
        /// Provider id or alias.
        provider: String,
        /// Label, or 1-based position in `auth status <provider>`.
        #[arg(value_name = "LABEL|INDEX")]
        credential: String,
    },
    /// Move an API key of the pool to a 0-based position (the order
    /// `fill_first` takes them in); the others are renumbered.
    Priority {
        /// Provider id or alias.
        provider: String,
        /// Label, or 1-based position in `auth status <provider>`.
        #[arg(value_name = "LABEL|INDEX")]
        credential: String,
        /// The new 0-based position.
        position: usize,
    },
    /// Clear benches and cooldowns (every credential of the provider, or
    /// one); running sessions use them on their next request.
    Reset {
        /// Provider id or alias.
        provider: String,
        /// Label, or 1-based position in `auth status <provider>`.
        #[arg(value_name = "LABEL|INDEX")]
        credential: Option<String>,
    },
    /// Refresh sign-in tokens now and return them to rotation.
    Refresh {
        /// Provider id or alias.
        provider: String,
        /// Label, or 1-based position in `auth status <provider>`.
        #[arg(value_name = "LABEL|INDEX")]
        credential: Option<String>,
    },
    /// Remove every subscription sign-in of a provider.
    Logout {
        /// Provider id or alias.
        provider: String,
    },
    /// Show a subscription's account usage windows without starting a
    /// session (exit 1 when nothing can be shown).
    Usage {
        /// Provider id or alias; default `model.provider`.
        #[arg(long, value_name = "ID")]
        provider: Option<String>,
    },
    /// Copy another tool's sign-in into the secrets store.
    Import {
        /// Where the sign-in comes from.
        #[arg(value_enum)]
        source: ImportSourceArg,
        /// The sign-in file (default `~/.codex/auth.json`); only read.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
        /// Label of the imported sign-in (default `default`).
        #[arg(long, value_name = "LABEL")]
        label: Option<String>,
    },
}

/// `auth add --type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AuthTypeArg {
    /// An API key.
    #[value(name = "api_key")]
    ApiKey,
    /// A subscription or OAuth sign-in.
    Oauth,
}

/// `auth import` sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ImportSourceArg {
    /// The Codex CLI's `auth.json` (provider `openai-codex`).
    CodexCli,
}
