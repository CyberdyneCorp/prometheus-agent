//! `prometheus config` arguments (spec: configuration-and-profiles).

use clap::Subcommand;

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
    /// List config keys naming a deprecated or retired model with their
    /// replacements; nothing changes without --apply.
    FixModels {
        /// Rewrite the keys (config.toml is first copied to
        /// config.toml.bak-<timestamp>).
        #[arg(long)]
        apply: bool,
        /// With --apply, skip the config.toml.bak-<timestamp> copy.
        #[arg(long, requires = "apply")]
        no_backup: bool,
    },
    /// Print the config.toml JSON Schema (draft 2020-12).
    Schema,
    /// Print the configuration reference (Markdown).
    Docs,
}
