//! `prometheus tools` and `prometheus toolsets` arguments (spec:
//! tool-system).

use clap::Subcommand;

/// `prometheus tools` subcommands.
#[derive(Debug, Subcommand)]
pub enum ToolsCommand {
    /// Enable or disable toolsets per surface and choose tool providers,
    /// interactively.
    Configure {
        /// The surface: cli, tui, cron, acp or gateway.<platform> (every
        /// surface when omitted).
        #[arg(long, value_name = "SURFACE")]
        surface: Option<String>,
    },
    /// Enable a toolset from the next session.
    Enable {
        /// The toolset.
        toolset: String,
        /// Only for this surface.
        #[arg(long, value_name = "SURFACE")]
        surface: Option<String>,
    },
    /// Disable a toolset from the next session.
    Disable {
        /// The toolset.
        toolset: String,
        /// Only for this surface.
        #[arg(long, value_name = "SURFACE")]
        surface: Option<String>,
    },
    /// Choose the provider of a tool category (web search, web extract,
    /// browser, image generation, TTS, STT, video, X search).
    Provider {
        /// The category.
        category: String,
        /// The provider.
        provider: String,
    },
    /// Every registered tool with its toolset and exposure verdict (`exposed`
    /// or a reason code) for the profile's configured toolsets, or as a
    /// session froze them (`--session`).
    List(ToolsListArgs),
    /// A tool's schema, owner, capabilities, effect, claims, timeout, result
    /// budget and the approval rules that apply to it.
    #[command(visible_alias = "show")]
    Explain {
        /// The tool name.
        name: String,
    },
    /// The toolsets, as `prometheus toolsets list`.
    Toolsets(ToolsetsListArgs),
}

/// `prometheus tools list` arguments.
#[derive(Debug, clap::Args)]
pub struct ToolsListArgs {
    /// Show the surface this session froze when it started (id, id prefix
    /// or title) instead of the one a new session would get.
    #[arg(long, value_name = "SESSION")]
    pub session: Option<String>,
}

/// `prometheus toolsets` subcommands.
#[derive(Debug, Subcommand)]
pub enum ToolsetsCommand {
    /// Every toolset: description, direct tools, includes, resolved tool
    /// count, default-off and posture flags, and whether it is active.
    List(ToolsetsListArgs),
    /// The tools a toolset resolves to, in order, with their availability.
    Show {
        /// The toolset name.
        name: String,
    },
}

/// Arguments of `toolsets list`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct ToolsetsListArgs {
    /// Mark the toolsets active on this surface (`toolsets.surfaces.<s>`,
    /// else `tools.enabled_toolsets`).
    #[arg(long, value_name = "SURFACE")]
    pub surface: Option<String>,
}
