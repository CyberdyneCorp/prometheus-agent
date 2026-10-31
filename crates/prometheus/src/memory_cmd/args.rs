//! The `prometheus memory` command tree.

use std::path::PathBuf;

use clap::{Subcommand, ValueEnum};

/// A built-in store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TargetArg {
    /// The agent's own notes.
    Memory,
    /// The user profile.
    User,
}

/// A store, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ResetTarget {
    /// Both stores.
    All,
    /// The agent's own notes.
    Memory,
    /// The user profile.
    User,
}

/// An entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    /// A fact.
    Fact,
    /// A preference.
    Preference,
    /// A standing instruction.
    Instruction,
    /// A project.
    Project,
    /// A person.
    Person,
    /// An event.
    Event,
}

/// A tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TierArg {
    /// Shown in the system prompt (budgeted).
    Core,
    /// Recalled per turn.
    Archival,
}

/// An export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    /// The Markdown view `memory edit` opens.
    Md,
    /// One JSON object per entry.
    Jsonl,
}

/// `prometheus memory` subcommands.
#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    /// Show whether built-in memory is enabled, each store's usage and pending changes.
    Status,
    /// List entries (dormant ones only with --all).
    List {
        /// Only this store.
        #[arg(long)]
        target: Option<TargetArg>,
        /// Only this kind.
        #[arg(long)]
        kind: Option<KindArg>,
        /// Only this tier.
        #[arg(long)]
        tier: Option<TierArg>,
        /// Include dormant entries.
        #[arg(long)]
        all: bool,
    },
    /// Show one entry in full (blocked entries show their raw text here).
    Show {
        /// The entry id (m_...).
        id: String,
    },
    /// Add an entry (from the next session's snapshot).
    Add {
        /// The text.
        content: String,
        /// The store.
        #[arg(long, default_value = "memory")]
        target: TargetArg,
        /// The kind.
        #[arg(long, default_value = "fact")]
        kind: KindArg,
        /// The tier.
        #[arg(long, default_value = "core")]
        tier: TierArg,
    },
    /// Remove an entry.
    Rm {
        /// The entry id.
        id: String,
    },
    /// Pin an entry.
    Pin {
        /// The entry id.
        id: String,
    },
    /// Unpin an entry.
    Unpin {
        /// The entry id.
        id: String,
    },
    /// Edit entries as Markdown in $VISUAL / $EDITOR; the result is applied as one batch after showing the diff.
    Edit {
        /// Only this store.
        #[arg(long)]
        target: Option<TargetArg>,
    },
    /// Print every entry as Markdown (the edit view) or JSONL.
    Export {
        /// The format.
        #[arg(long, default_value = "md")]
        format: FormatArg,
        /// Only this store.
        #[arg(long)]
        target: Option<TargetArg>,
        /// Write to this file instead of stdout.
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Import a Markdown or JSONL export, or a Hermes MEMORY.md / USER.md (entries separated by §).
    Import {
        /// The file.
        file: PathBuf,
        /// The store a Hermes file belongs to (default: from the file name).
        #[arg(long)]
        target: Option<TargetArg>,
        /// The tier of entries from a Hermes file.
        #[arg(long, default_value = "core")]
        tier: TierArg,
    },
    /// List memory changes staged for approval.
    Pending,
    /// Apply a staged change (`all` for every one).
    Approve {
        /// The pending id, or `all`.
        id: String,
    },
    /// Drop a staged change (`all` for every one).
    Reject {
        /// The pending id, or `all`.
        id: String,
    },
    /// Delete every entry of a store.
    Reset {
        /// Which store.
        #[arg(long, default_value = "all")]
        target: ResetTarget,
        /// Do not ask.
        #[arg(long)]
        yes: bool,
    },
}
