//! The clap command tree. The full tree and the remaining global flags
//! arrive with M1 group 19; this holds what the process contracts need now.

use std::path::PathBuf;

use clap::{ArgAction, CommandFactory, Parser, Subcommand};

use crate::build_info;

// The command name is fixed rather than taken from `argv[0]`, so the `pmt`
// alias prints exactly the same identity line as `prometheus`. `long_about`
// is cleared so this type's doc comment never leaks into `--help`.
/// Parsed command line.
#[derive(Debug, Parser)]
#[command(name = "prometheus", bin_name = "prometheus", version = build_info::IDENTITY)]
#[command(about = "Prometheus Agent", long_about = None)]
pub struct Cli {
    /// Prometheus home directory.
    #[arg(long, global = true, value_name = "PATH")]
    #[arg(help = "Prometheus home directory (default: PROMETHEUS_HOME, else ~/.prometheus)")]
    pub home: Option<PathBuf>,
    /// Profile to use.
    #[arg(short = 'p', long, global = true, value_name = "NAME")]
    #[arg(help = "Profile to use (default: the home's active_profile, else default)")]
    pub profile: Option<String>,
    /// `key=value` config overrides for this invocation only.
    #[arg(long = "set", global = true, value_name = "KEY=VALUE")]
    #[arg(help = "Override a config key for this invocation only; repeatable, never saved")]
    pub set: Vec<String>,
    /// Print one JSON document on stdout; errors go to stderr as JSON.
    #[arg(long, global = true)]
    pub json: bool,
    /// More detail; repeatable.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
    /// The subcommand; `chat` when omitted.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the version and build identity (`--json` for every field,
    /// `--verbose` for build details and compiled-in surfaces).
    Version,
    /// Read, change, validate and explain the profile's config.toml.
    Config {
        /// The config subcommand.
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

/// `prometheus config` subcommands.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the path of the profile's config.toml.
    Path,
    /// Print config.toml, or with --effective every effective value.
    Show {
        /// Every effective value (defaults, file and --set) instead of the file.
        #[arg(long)]
        effective: bool,
    },
    /// Print the effective value of a key.
    Get {
        /// Dotted key, for example `agent.max_iterations`.
        #[arg(help = "Dotted key, for example agent.max_iterations")]
        key: String,
    },
    /// Set a key in config.toml (the value is typed by the key's schema).
    Set {
        /// Dotted key, for example display.theme.
        key: String,
        /// New value: text for string keys, else a TOML value (`10`, `true`, `["a"]`).
        #[arg(allow_hyphen_values = true)]
        #[arg(help = "New value: text for string keys, else a TOML value (10, true, [\"a\"])")]
        value: String,
    },
    /// Remove a key from config.toml so its default applies.
    Unset {
        /// Dotted key.
        key: String,
    },
    /// Edit config.toml in $VISUAL / $EDITOR; the result is validated before it is saved.
    Edit,
    /// Validate config.toml (exit 0 when valid, 78 when not).
    Validate,
    /// Explain a key: type, default, effective value, source, apply class.
    #[command(arg_required_else_help = true)]
    Explain {
        /// Dotted key.
        #[arg(required_unless_present = "changed")]
        key: Option<String>,
        /// List every key whose effective value differs from its default.
        #[arg(long, conflicts_with = "key")]
        changed: bool,
    },
    /// Print the config.toml JSON Schema (draft 2020-12).
    Schema,
    /// Print the configuration reference (Markdown).
    Docs,
}

/// The clap command tree, for generated documentation.
#[must_use]
pub fn command() -> clap::Command {
    Cli::command()
}
