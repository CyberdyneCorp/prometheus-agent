//! `prometheus model` and `prometheus fallback` arguments (spec:
//! model-providers).

use clap::Subcommand;

/// `prometheus model` subcommands.
#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Show each step resolving the provider, endpoint, wire format,
    /// credential and model, without calling the model or printing secrets.
    Explain {
        /// Provider id or alias (as `--provider` on a session).
        #[arg(long, value_name = "ID")]
        provider: Option<String>,
        /// Model id (as `--model` on a session).
        #[arg(long, value_name = "ID")]
        model: Option<String>,
        /// Launch-only endpoint.
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Validate a route (credential, tool support, context window) and
    /// make it the profile's model.
    Set {
        /// `<provider>/<model>` or a `[model.aliases]` name.
        #[arg(value_name = "PROVIDER/MODEL")]
        model: String,
        /// Also set `providers.<id>.wire_format`.
        #[arg(long, value_enum, value_name = "FORMAT")]
        wire_format: Option<WireFormatArg>,
        /// Also set `providers.<id>.base_url` (a local server root is
        /// normalised to the prefix that serves its model listing).
        #[arg(long, value_name = "URL")]
        base_url: Option<String>,
    },
    /// List models with their context window and capabilities.
    List {
        /// Only this provider's models.
        #[arg(long, value_name = "ID")]
        provider: Option<String>,
        /// List providers (ids, aliases, wire format, endpoint, secret
        /// names) instead of models.
        #[arg(long, conflicts_with = "provider")]
        providers: bool,
        /// Fetch live listings and the models.dev catalog first.
        #[arg(long)]
        refresh: bool,
    },
    /// Show the profile's current route, context window and reasoning
    /// effort.
    Info,
}

/// `--wire-format` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum WireFormatArg {
    /// OpenAI Chat Completions.
    ChatCompletions,
    /// OpenAI Responses.
    Responses,
    /// Anthropic Messages.
    AnthropicMessages,
}

/// A launch-only endpoint (`--base-url`, `--api-key-stdin`): used for this
/// invocation only and never saved.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct EndpointArgs {
    /// Endpoint for this invocation only (without `--provider`: provider
    /// `custom`); never saved.
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,
    /// Read the endpoint's API key from stdin; it is held in memory only
    /// and sent to the `--base-url` host only.
    #[arg(long, requires = "base_url")]
    pub api_key_stdin: bool,
}

/// `prometheus fallback` subcommands.
#[derive(Debug, Subcommand)]
pub enum FallbackCommand {
    /// List the chain in order.
    List,
    /// Validate a route and append it (no argument: the model picker).
    Add {
        /// `<provider>/<model>` or a `[model.aliases]` name.
        #[arg(value_name = "PROVIDER/MODEL")]
        model: Option<String>,
    },
    /// Remove an entry by its 1-based position or `<provider>/<model>`.
    Remove {
        /// Position or `<provider>/<model>`.
        #[arg(value_name = "INDEX|PROVIDER/MODEL")]
        entry: String,
    },
    /// Remove every entry.
    Clear,
}
