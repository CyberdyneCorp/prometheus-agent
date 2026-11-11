//! `prometheus setup` arguments (spec: cli, Setup wizard; Setup wizard
//! modes).

use clap::{Args, ValueEnum};

/// `prometheus setup` arguments.
#[derive(Debug, Clone, Args)]
#[allow(clippy::struct_excessive_bools)] // independent modes of one command line
pub struct SetupArgs {
    /// Only this section (all but `tools` and `gateway` when omitted).
    #[arg(value_enum)]
    pub section: Option<SetupSection>,
    /// `setup gateway <platform>`: the messaging adapter to set up.
    #[arg(value_name = "PLATFORM")]
    pub platform: Option<String>,
    /// Take values from `--set` and secrets from the environment instead
    /// of asking; a missing required value exits 78.
    #[arg(long)]
    pub non_interactive: bool,
    /// Ask only for the values the profile has not set yet.
    #[arg(long, conflicts_with_all = ["non_interactive", "reset"])]
    pub quick: bool,
    /// Restore the section's settings to their defaults (after a
    /// confirmation; secrets are kept).
    #[arg(long, conflicts_with = "non_interactive")]
    pub reset: bool,
    /// Clear the onboarding state (offered setup, shown tips, tour steps).
    #[arg(long, conflicts_with_all = ["section", "non_interactive", "quick", "reset"])]
    pub reset_onboarding: bool,
    /// Sign in to the Nous Portal, make `nous` (and its default model) the
    /// main route and send web, image and speech tools through its tool
    /// gateway, showing the change before writing it.
    #[arg(long, conflicts_with_all = ["section", "non_interactive", "reset_onboarding", "quick", "reset"])]
    pub portal: bool,
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
    /// Text to speech: the provider and its API key.
    Tts,
    /// The agent: identity file, approval mode and iteration budget.
    Agent,
    /// Messaging adapters (with the gateway).
    Gateway,
}
