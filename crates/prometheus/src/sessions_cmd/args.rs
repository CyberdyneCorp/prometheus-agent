//! `prometheus sessions` arguments (spec: session-storage).

use std::path::PathBuf;

use clap::Subcommand;
use pmt_store::{Role, Source};

/// `prometheus sessions` subcommands.
#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    /// Pick a session interactively (status tags, search) and resume it.
    Browse {
        /// Only sessions from this source.
        #[arg(long, value_name = "SOURCE", value_parser = parse_source)]
        source: Option<Source>,
        /// Also hidden, archived and integration (oneshot, tool) sessions.
        #[arg(long)]
        all: bool,
    },
    /// Resume a session (id, id prefix or title) in a chat; a turn the
    /// process stopped during is resumed, closed or offered by
    /// `agent.resume_on_crash`.
    Resume {
        /// Session id, id prefix or title.
        session: String,
    },
    /// List sessions, most recently active first.
    List {
        /// At most this many sessions.
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
        /// Also hidden, archived and integration (oneshot, tool) sessions.
        #[arg(long)]
        all: bool,
        /// Only sessions from this source (repeatable).
        #[arg(long, value_name = "SOURCE", value_parser = parse_source)]
        source: Vec<Source>,
        /// Only pinned sessions.
        #[arg(long)]
        pinned: bool,
    },
    /// Show a session (id, id prefix or title).
    Show {
        /// Session id, id prefix (8+ characters) or title.
        session: String,
    },
    /// Full-text search over the profile's messages.
    Search(SearchArgs),
    /// Set a session's title.
    Rename {
        /// Session id, id prefix or title.
        session: String,
        /// The new title (at most 100 characters).
        title: String,
    },
    /// Pin a session (pinned sessions are never pruned).
    Pin {
        /// Session id, id prefix or title.
        session: String,
    },
    /// Unpin a session.
    Unpin {
        /// Session id, id prefix or title.
        session: String,
    },
    /// Archive a session (hidden from the default listing).
    Archive {
        /// Session id, id prefix or title.
        session: String,
    },
    /// Unarchive a session.
    Unarchive {
        /// Session id, id prefix or title.
        session: String,
    },
    /// Show a session's lineage tree (forks, continuations, delegates).
    Tree {
        /// Session id, id prefix or title.
        session: String,
    },
    /// Fork a session: a new session with its active messages up to --at.
    Fork {
        /// Session id, id prefix or title.
        session: String,
        /// The last message the fork keeps (default: the last active one).
        #[arg(long, value_name = "MESSAGE")]
        at: Option<String>,
        /// The fork's title (default: `<title> #<n>`).
        #[arg(long)]
        title: Option<String>,
    },
    /// List the other children of a session's parent (forks, continuations).
    Siblings {
        /// Session id, id prefix or title.
        session: String,
    },
    /// Import a JSONL export (all-or-none; ids must be new unless --remap-ids).
    Import {
        /// The export file.
        file: PathBuf,
        /// Give every session, message and usage row a new id.
        #[arg(long)]
        remap_ids: bool,
    },
    /// Copy every readable row of state.db into a new clean store (needs the store closed).
    Recover {
        /// Write the recovered store here instead of next to state.db (no swap).
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Swap the recovered store in without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Switch state.db's journal mode offline (delete for filesystems without shared memory).
    JournalMode {
        /// The journal mode.
        mode: JournalModeArg,
    },
    /// Rebuild the stored system prompt of sessions at their next turn.
    RefreshPrompts {
        /// Sessions (id, id prefix or title).
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        sessions: Vec<String>,
        /// Every resumable session.
        #[arg(long)]
        all: bool,
    },
    /// Regenerate automatic titles with the title model (user titles are kept).
    Retitle {
        /// Sessions (id, id prefix or title).
        #[arg(conflicts_with_all = ["all", "auto_only"])]
        sessions: Vec<String>,
        /// Every session not titled by the user (untitled ones too).
        #[arg(long)]
        all: bool,
        /// Only sessions that already have an automatic title.
        #[arg(long)]
        auto_only: bool,
    },
    /// Show a session's and its lineage's message and token totals, or the
    /// whole profile's with --profile-total.
    Stats {
        /// Session id, id prefix or title.
        #[arg(
            required_unless_present = "profile_total",
            conflicts_with = "profile_total"
        )]
        session: Option<String>,
        /// The totals of every session of the profile (imported history
        /// included).
        #[arg(long)]
        profile_total: bool,
    },
    /// Export sessions (jsonl, md, html, prompts or trace).
    Export(ExportArgs),
    /// Delete a session (asks for confirmation unless --yes).
    Delete {
        /// Session id, id prefix or title.
        session: String,
        /// Also delete its compression continuations.
        #[arg(long)]
        with_lineage: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Delete ended, unpinned sessions inactive for longer than --older-than days.
    Prune {
        /// Inactivity threshold in days.
        #[arg(long, value_name = "DAYS")]
        older_than: u64,
        /// Only print what would be deleted.
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Rewind a session to a user message (soft delete; --restore undoes it).
    Rewind {
        /// Session id, id prefix or title.
        session: String,
        /// The user message to rewind to (default: the last one).
        #[arg(long, value_name = "MESSAGE_ID", conflicts_with = "restore")]
        to: Option<String>,
        /// Restore the rewind that hid this message.
        #[arg(long, value_name = "MESSAGE_ID")]
        restore: Option<String>,
    },
    /// Rebuild the derived search indexes from the stored messages.
    Repair {
        /// Rebuild the search indexes (the only repair so far).
        #[arg(long)]
        indexes: bool,
    },
    /// Compact state.db and truncate its write-ahead log.
    Vacuum,
}

/// `prometheus sessions search` arguments.
#[derive(Debug, clap::Args)]
pub struct SearchArgs {
    /// The query: words, "quoted phrases", prefix* and AND/OR/NOT.
    pub query: String,
    /// Hits to return.
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub limit: u32,
    /// Hits to skip.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub offset: u32,
    /// Result order.
    #[arg(long, value_enum, default_value_t = SortArg::Rank)]
    pub sort: SortArg,
    /// Only these sources (repeatable).
    #[arg(long, value_name = "SOURCE", value_parser = parse_source)]
    pub source: Vec<Source>,
    /// Never these sources (repeatable).
    #[arg(long, value_name = "SOURCE", value_parser = parse_source)]
    pub exclude_source: Vec<Source>,
    /// Only these roles (repeatable: user, assistant, tool, system).
    #[arg(long, value_name = "ROLE", value_parser = parse_role)]
    pub role: Vec<Role>,
    /// Only messages at or after this time (RFC 3339 or YYYY-MM-DD).
    #[arg(long, value_name = "TIME", value_parser = parse_time)]
    pub after: Option<jiff::Timestamp>,
    /// Only messages before this time (RFC 3339 or YYYY-MM-DD).
    #[arg(long, value_name = "TIME", value_parser = parse_time)]
    pub before: Option<jiff::Timestamp>,
    /// Only this session (id, id prefix or title).
    #[arg(long, value_name = "SESSION")]
    pub session: Option<String>,
    /// Only pinned sessions.
    #[arg(long)]
    pub pinned: bool,
    /// Leave archived sessions out.
    #[arg(long)]
    pub no_archived: bool,
    /// Include rewound messages.
    #[arg(long)]
    pub include_inactive: bool,
}

/// `prometheus sessions export` arguments.
#[derive(Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)] // independent command-line switches
pub struct ExportArgs {
    /// Sessions to export (id, id prefix or title).
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub sessions: Vec<String>,
    /// Export every session.
    #[arg(long)]
    pub all: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t = ExportFormat::Jsonl)]
    pub format: ExportFormat,
    /// Include rewound and compacted messages.
    #[arg(long)]
    pub include_inactive: bool,
    /// Embed the blobs (attachments) the messages reference, base64 (jsonl).
    #[arg(long)]
    pub with_blobs: bool,
    /// Replace secrets and credential-shaped text with [REDACTED].
    #[arg(long)]
    pub redact: bool,
    /// Write to this file instead of stdout.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

/// SQLite journal modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum JournalModeArg {
    /// Write-ahead log (default).
    Wal,
    /// Rollback journal (filesystems without shared-memory support).
    Delete,
}

/// Search result order on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SortArg {
    /// Best match first.
    Rank,
    /// Newest first.
    Newest,
    /// Oldest first.
    Oldest,
}

/// Session export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ExportFormat {
    /// Portable JSONL (header, session, messages, usage, lineage).
    Jsonl,
    /// Readable Markdown transcript, tool calls collapsed.
    Md,
    /// Self-contained HTML page.
    Html,
    /// Only the user messages, one JSON line each.
    Prompts,
    /// Trajectory grammar JSONL.
    Trace,
}

fn parse_source(text: &str) -> Result<Source, String> {
    text.parse()
        .map_err(|e: pmt_core::Error| e.message().to_owned())
}

fn parse_role(text: &str) -> Result<Role, String> {
    text.parse()
        .map_err(|e: pmt_core::Error| e.message().to_owned())
}

/// RFC 3339, or a date meaning its UTC midnight.
fn parse_time(text: &str) -> Result<jiff::Timestamp, String> {
    if let Ok(ts) = text.parse::<jiff::Timestamp>() {
        return Ok(ts);
    }
    text.parse::<jiff::civil::Date>()
        .and_then(|date| date.to_zoned(jiff::tz::TimeZone::UTC))
        .map(|zoned| zoned.timestamp())
        .map_err(|_| format!("'{text}' is not an RFC 3339 time or a YYYY-MM-DD date"))
}
