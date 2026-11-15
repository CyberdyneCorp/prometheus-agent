//! `prometheus harness` arguments.

use std::path::PathBuf;

use clap::Subcommand;

/// `prometheus harness` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum HarnessCommand {
    /// Validate a harness tree (default: the active version) and print
    /// every error; exits 65 when it is invalid.
    Validate {
        /// The tree's directory.
        dir: Option<PathBuf>,
    },
    /// Harness versions, newest first; `*` marks the active one.
    Log,
    /// A version's files (default: the active version), or one file.
    Show {
        /// `h<n>` or the full version id.
        version: Option<String>,
        /// A file of the tree.
        path: Option<String>,
    },
    /// The unified diff and summary from version A to B (default: the
    /// active version).
    Diff {
        /// The older version.
        a: String,
        /// The newer version.
        b: Option<String>,
    },
    /// Activate a new version with an earlier version's tree (versions
    /// are never deleted).
    Rollback {
        /// The version to return to.
        version: String,
        /// Rebuild open sessions' prompts at their next turn (breaks
        /// their prompt cache) instead of waiting for the next session.
        #[arg(long)]
        now: bool,
    },
    /// Validate an edited tree and activate it as a new manual version.
    Apply {
        /// The tree's directory.
        dir: PathBuf,
        /// Rebuild open sessions' prompts at their next turn.
        #[arg(long)]
        now: bool,
    },
    /// Edit a copy of the active version in $EDITOR; a valid change
    /// becomes a new manual version.
    Edit {
        /// The file to open.
        #[arg(default_value = "SYSTEM.md")]
        path: String,
        /// Rebuild open sessions' prompts at their next turn.
        #[arg(long)]
        now: bool,
    },
}
