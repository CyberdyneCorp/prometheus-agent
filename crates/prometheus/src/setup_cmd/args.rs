//! `prometheus setup` arguments (spec: cli).

use clap::{Args, ValueEnum};

/// `prometheus setup` arguments.
#[derive(Debug, Clone, Args)]
pub struct SetupArgs {
    /// Only this section (all but `tools` and `gateway` when omitted).
    #[arg(value_enum)]
    pub section: Option<SetupSection>,
    /// Take values from `--set` and secrets from the environment instead
    /// of asking; a missing required value exits 78.
    #[arg(long)]
    pub non_interactive: bool,
    /// Clear the onboarding state (offered setup, shown tips, tour steps).
    #[arg(long, conflicts_with_all = ["section", "non_interactive"])]
    pub reset_onboarding: bool,
}

/// Sections of `prometheus setup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SetupSection {
    /// Provider and model, or a subscription sign-in, and its API key.
    Model,
    /// The secrets the configured providers need.
    Secrets,
    /// Toolsets per surface and tool providers.
    Tools,
    /// The terminal backend.
    Terminal,
    /// The memory stores.
    Memory,
    /// Messaging adapters (with the gateway).
    Gateway,
}
