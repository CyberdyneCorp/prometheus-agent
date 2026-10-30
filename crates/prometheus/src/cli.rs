//! The clap command tree. The full tree and the remaining global flags
//! arrive with M1 group 19; this holds what the process contracts need now.

use std::path::PathBuf;

use clap::{ArgAction, CommandFactory, Parser, Subcommand};

use pmt_store::{Role, Source};

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
    /// Store, list and remove the profile's secrets (values never on the command line).
    Secrets {
        /// The secrets subcommand.
        #[command(subcommand)]
        command: SecretsCommand,
    },
    /// Back up the whole home to a .tar.zst archive, or restore one.
    Backup {
        /// The backup subcommand.
        #[command(subcommand)]
        command: BackupCommand,
    },
    /// Snapshot and restore the selected profile's critical state.
    Snapshots {
        /// The snapshots subcommand.
        #[command(subcommand)]
        command: SnapshotsCommand,
    },
    /// List, create, select, rename and delete profiles.
    Profile {
        /// The profile subcommand.
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// List, search, export and manage the profile's sessions.
    Sessions {
        /// The sessions subcommand.
        #[command(subcommand)]
        command: SessionsCommand,
    },
}

/// `prometheus sessions` subcommands.
#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
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
    /// Show a session's and its lineage's message and token totals.
    Stats {
        /// Session id, id prefix or title.
        session: String,
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
    /// Replace secrets and credential-shaped text with [REDACTED].
    #[arg(long)]
    pub redact: bool,
    /// Write to this file instead of stdout.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,
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

/// `prometheus secrets` subcommands.
#[derive(Debug, Subcommand)]
pub enum SecretsCommand {
    /// Store a secret; the value is read from a hidden prompt or from stdin.
    Set {
        /// Secret name, for example `OPENAI_API_KEY`.
        name: String,
        /// Refused: values are never accepted as arguments.
        #[arg(hide = true, num_args = 0.., allow_hyphen_values = true)]
        value: Vec<String>,
    },
    /// List secret names, their source and whether they are shadowed (never values).
    List,
    /// Show where a secret comes from; --reveal prints the value to a terminal only.
    Get {
        /// Secret name.
        name: String,
        /// Print the value (refused unless stdout is a terminal).
        #[arg(long)]
        reveal: bool,
    },
    /// Remove a secret from the profile's store.
    #[command(visible_alias = "rm")]
    Unset {
        /// Secret name.
        name: String,
    },
    /// Move every secret to another backend and select it in config.toml.
    Migrate {
        /// Target backend.
        #[arg(long, value_enum)]
        to: SecretsBackendArg,
    },
}

/// A secrets backend named on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SecretsBackendArg {
    /// `<profile>/secrets.toml` (mode 0600).
    File,
    /// The OS keychain.
    Keychain,
}

/// `prometheus backup` subcommands.
#[derive(Debug, Subcommand)]
pub enum BackupCommand {
    /// Write a backup of the whole home (default: <home>/backups/prometheus-<UTC time>.tar.zst).
    Create {
        /// Write the archive here instead.
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Only critical state: config, secrets, grants, state.db, cron jobs, pairing data.
        #[arg(long)]
        quick: bool,
        /// Label added to the default file name.
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
        /// Default-named backups to keep in <home>/backups.
        #[arg(long, value_name = "N", default_value_t = 3)]
        keep: usize,
    },
    /// Restore an archive after verifying every member (-p/--profile NAME restores only that profile).
    Restore {
        /// The .tar.zst archive.
        archive: PathBuf,
        /// Replace existing data without asking.
        #[arg(long)]
        force: bool,
    },
}

/// `prometheus snapshots` subcommands.
#[derive(Debug, Subcommand)]
pub enum SnapshotsCommand {
    /// List the profile's snapshots, newest first.
    List,
    /// Snapshot the profile's critical state now.
    Create {
        /// A label for the snapshot.
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
    },
    /// Restore a snapshot (a pre-restore snapshot is taken first).
    Restore {
        /// Snapshot id (`snap_...`).
        id: String,
    },
    /// Remove the oldest manual snapshots beyond --keep (default: snapshots.keep).
    Prune {
        /// Manual snapshots to keep.
        #[arg(long, value_name = "N")]
        keep: Option<usize>,
    },
}

/// `prometheus profile` subcommands.
#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// List the profiles; the active one is marked.
    List,
    /// Create a profile, optionally copying another one once.
    Create {
        /// New profile name (lowercase letters, digits, '_' and '-').
        name: String,
        /// Copy config, identity, memory, skills, themes and plugins from this profile.
        #[arg(long, value_name = "NAME")]
        clone_from: Option<String>,
        /// With --clone-from, also copy the secrets.
        #[arg(long, requires = "clone_from")]
        clone_secrets: bool,
    },
    /// Make a profile the one used without -p (`use default` clears the choice).
    Use {
        /// Profile name.
        name: String,
    },
    /// Show a profile (default: the selected one).
    Show {
        /// Profile name.
        name: Option<String>,
    },
    /// Rename a profile.
    Rename {
        /// Current name.
        old: String,
        /// New name.
        new: String,
    },
    /// Delete a profile and everything in it (asks to type its name unless --yes).
    Delete {
        /// Profile name.
        name: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
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
