//! The clap command tree (spec: cli, Top-level command tree): the global
//! flags, `chat` as the default command (its flags are accepted without
//! the subcommand) and every subcommand that has landed.

use std::path::PathBuf;

use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand};

pub use crate::auth_cmd::args::{AuthCommand, AuthTypeArg, ImportSourceArg};
pub use crate::chat::args::ChatArgs;
pub use crate::config_cmd::args::ConfigCommand;
pub use crate::doctor_cmd::args::DoctorArgs;
pub use crate::memory_cmd::args::MemoryCommand;
pub use crate::model_cmd::args::{EndpointArgs, FallbackCommand, ModelCommand, WireFormatArg};
pub use crate::runtimes_cmd::args::{ExcludeCommand, RuntimesCommand};
pub use crate::sessions_cmd::args::{
    ExportArgs, ExportFormat, SearchArgs, SessionsCommand, SortArg,
};
pub use crate::setup_cmd::args::{SetupArgs, SetupSection};
pub use crate::skills_cmd::args::{BundlesCommand, PendingCommand, SkillsCommand};
pub use crate::tools_cmd::args::{ToolsCommand, ToolsListArgs, ToolsetsCommand, ToolsetsListArgs};

// The command name is fixed rather than taken from `argv[0]`, so the `pmt`
// alias prints exactly the same identity line as `prometheus`. `long_about`
// is cleared so this type's doc comment never leaks into `--help`.
/// Parsed command line.
#[derive(Debug, Parser)]
// `-V/--version` is a plain flag rather than clap's, so `--version
// --verbose` can print the build details (and the runtime index digest).
#[command(
    name = "prometheus",
    bin_name = "prometheus",
    disable_version_flag = true
)]
#[command(about = "Prometheus Agent", long_about = None)]
#[allow(clippy::struct_excessive_bools)] // independent global switches
pub struct Cli {
    /// Prometheus home directory.
    #[arg(long, global = true, value_name = "PATH")]
    #[arg(help = "Prometheus home directory (default: PROMETHEUS_HOME, else ~/.prometheus)")]
    pub home: Option<PathBuf>,
    /// Profile to use (`migrate from-hermes` takes it repeatedly, naming
    /// Hermes profiles).
    #[arg(short = 'p', long, global = true, value_name = "NAME", action = ArgAction::Append)]
    #[arg(help = "Profile to use (default: the home's active_profile, else default)")]
    pub profile: Vec<String>,
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
    /// No colour (also when `NO_COLOR` is set or stdout is not a terminal).
    #[arg(long, global = true)]
    pub no_color: bool,
    /// Only the answer: no banner, spinner or tool previews.
    #[arg(short, long, global = true)]
    pub quiet: bool,
    /// Recovery start (spec: plugin-system, Safe mode).
    #[arg(long, global = true)]
    #[arg(
        help = "Start with no plugins, MCP servers, sidecars, hooks or webhooks; a chat also ignores config.toml and rules and needs --model and --provider"
    )]
    pub safe_mode: bool,
    /// Run shell hooks nobody approved yet (as `hooks.auto_accept`).
    #[arg(long, global = true)]
    pub accept_hooks: bool,
    /// `-V/--version`: the identity line (`--verbose`: as `version --verbose`).
    #[arg(short = 'V', long = "version", help = "Print version")]
    pub print_version: bool,
    /// `chat` flags, when the subcommand is omitted.
    #[command(flatten)]
    pub chat: ChatArgs,
    /// The subcommand; `chat` when omitted.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once per process; boxing buys nothing
pub enum Command {
    /// Talk to the agent: interactive on a terminal, one-shot with `-z`
    /// or when stdin or stdout is not a terminal (the default command).
    Chat(ChatArgs),
    /// Print the version and build identity (`--json` for every field,
    /// `--verbose` for build details and compiled-in surfaces).
    Version,
    /// Set up the profile: model and provider (or a subscription sign-in),
    /// secrets, tools, terminal and memory.
    Setup(SetupArgs),
    /// Check the install and profile: one ok / warn / fail line per check;
    /// `--fix` performs the enumerated repairs.
    Doctor(DoctorArgs),
    /// Diagnostics: a setup summary for support threads, a redacted debug
    /// bundle, an opt-in report upload and a daemon task dump.
    Debug {
        /// The debug subcommand.
        #[command(subcommand)]
        command: DebugCommand,
    },
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
    /// List, inspect, enable, lint and maintain the profile's skills and skill bundles.
    Skills {
        /// The skills subcommand.
        #[command(subcommand)]
        command: SkillsCommand,
    },
    /// The skill curator: status, passes, pins, archive and restore, ledger and rollback.
    Curator {
        /// The curator subcommand.
        #[command(subcommand)]
        command: crate::curator_cmd::CuratorCommand,
    },
    /// The skills and memories the agent learned, as a timeline; list, delete, edit.
    #[command(visible_aliases = ["learning", "memory-graph"])]
    Journey(crate::journey_cmd::JourneyArgs),
    /// Inspect, edit, export and import what the agent remembers (built-in memory).
    Memory {
        /// The memory subcommand.
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// List and revoke the profile's permanent capability grants and approvals.
    Grants {
        /// The grants subcommand.
        #[command(subcommand)]
        command: GrantsCommand,
    },
    /// Browser sessions: status, and closing the real browser whose profile
    /// real-profile browsing copies.
    Browser {
        /// The browser subcommand.
        #[command(subcommand)]
        command: crate::browser_cmd::BrowserCommand,
    },
    /// Language servers behind the diagnostics of file edits: status,
    /// registry, installs, restart.
    Lsp {
        /// The lsp subcommand.
        #[command(subcommand)]
        command: crate::lsp_cmd::LspCommand,
    },
    /// Scheduled jobs: create, list, pause, run now, inspect runs, replay,
    /// doctor and the headless tick.
    Cron {
        /// The cron subcommand.
        #[command(subcommand)]
        command: crate::cron_cmd::CronCommand,
    },
    /// Git worktrees of sessions and subagents: list, sweep and restore
    /// archived trees.
    Worktrees {
        /// The worktrees subcommand.
        #[command(subcommand)]
        command: crate::worktrees_cmd::WorktreesCommand,
    },
    /// Projects: named workspaces spanning folders, the active project,
    /// archive and restore.
    Projects {
        /// The projects subcommand.
        #[command(subcommand)]
        command: crate::projects_cmd::ProjectsCommand,
    },
    /// Themes: list them, use one, set a colour, spinner or branding key
    /// of the custom theme, show the one in use.
    Themes {
        /// The themes subcommand.
        #[command(subcommand)]
        command: crate::themes_cmd::ThemesCommand,
    },
    /// A console that runs only read-mostly prometheus commands (status,
    /// doctor, logs, sessions list/show/export, config show/get/validate,
    /// usage, version, update --check, daemon status/restart, gateway
    /// status), never a shell.
    Console,
    /// Webhook routes kept in the profile's state: subscribe (prints the
    /// route URL and, once, its signing secret), list, remove and test.
    Webhooks {
        /// The webhooks subcommand.
        #[command(subcommand)]
        command: crate::webhooks_cmd::WebhooksCommand,
    },
    /// The credential vault: logins, cards and addresses the browser fills
    /// by handle.
    Vault {
        /// The vault subcommand.
        #[command(subcommand)]
        command: crate::vault_cmd::VaultCommand,
    },
    /// Security records of the profile: verify the tamper-evident audit log.
    Security {
        /// The security subcommand.
        #[command(subcommand)]
        command: SecurityCommand,
    },
    /// Back up the whole home to a .tar.zst archive, or restore one.
    Backup {
        /// The backup subcommand.
        #[command(subcommand)]
        command: BackupCommand,
    },
    /// Session recordings: export one, replay it (detecting divergence),
    /// or re-record it with this build.
    Replay {
        /// The replay subcommand.
        #[command(subcommand)]
        command: crate::replay_cmd::ReplayCommand,
    },
    /// The model-call cache: stats, prune, clear.
    Cache {
        /// The cache subcommand.
        #[command(subcommand)]
        command: crate::cache_cmd::CacheCommand,
    },
    /// Undo a session's file changes, roll a workspace back to a
    /// checkpoint, and manage the file snapshot store.
    Files {
        /// The files subcommand.
        #[command(subcommand)]
        command: crate::files_cmd::FilesCommand,
    },
    /// Detect a project's build/test/start recipe and run it.
    Verify(crate::verify_cmd::VerifyArgs),
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
    /// Show structured log records of a component (`list` names them).
    Logs(LogsArgs),
    /// Whether the daemon runs and how: components, sessions, the next
    /// scheduled job and today's spend (`--check` for scripts).
    Status(crate::status_cmd::StatusArgs),
    /// Totals of model and metered tool calls from the usage ledger.
    Usage(UsageArgs),
    /// Measure the system prompt and tool schemas a new session would send.
    Prompt {
        /// The prompt subcommand.
        #[command(subcommand)]
        command: PromptCommand,
    },
    /// Run the offline demo: the agent fixes a bug in a small ledger
    /// repository with the scripted mock provider, in a temporary profile.
    Demo(DemoArgs),
    /// Render a session's recorded trajectory, or export sessions as
    /// `ShareGPT` training records.
    Trajectory {
        /// The trajectory subcommand.
        #[command(subcommand)]
        command: TrajectoryCommand,
    },
    /// Pick, set, list and inspect the profile's model (no subcommand: the
    /// interactive picker).
    Model {
        /// The model subcommand.
        #[command(subcommand)]
        command: Option<ModelCommand>,
    },
    /// Provider routing state: per-route health scores.
    Providers {
        /// The providers subcommand.
        #[command(subcommand)]
        command: crate::providers_cmd::ProvidersCommand,
    },
    /// List, create, update and delete Mixture of Agents presets
    /// (`[moa.presets.<name>]`).
    Moa {
        /// The moa subcommand.
        #[command(subcommand)]
        command: crate::moa_cmd::MoaCommand,
    },
    /// Local models: the llama.cpp engine, the hardware-priced catalog,
    /// downloads, the model library and the managed llama-server.
    Local {
        /// The local subcommand.
        #[command(subcommand)]
        command: crate::local_cmd::LocalCommand,
    },
    /// List and edit the profile's fallback chain (`[[fallback.chain]]`).
    Fallback {
        /// The fallback subcommand.
        #[command(subcommand)]
        command: FallbackCommand,
    },
    /// Add, list and remove provider API keys and subscription sign-ins
    /// (an interactive manager without a subcommand on a terminal).
    Auth {
        /// The auth subcommand.
        #[command(subcommand)]
        command: Option<AuthCommand>,
    },
    /// List the profile's tools and explain one (exposure, capabilities, approvals).
    Tools {
        /// The tools subcommand.
        #[command(subcommand)]
        command: ToolsCommand,
    },
    /// List toolsets and show the tools a toolset resolves to.
    Toolsets {
        /// The toolsets subcommand.
        #[command(subcommand)]
        command: ToolsetsCommand,
    },
    /// Check how the approval gate treats a command, without running it.
    Approvals {
        /// The approvals subcommand.
        #[command(subcommand)]
        command: ApprovalsCommand,
    },
    /// Update this install to its channel's newest signed release, check
    /// for one (exit 10 when available), change the channel, or roll back.
    Update(UpdateArgs),
    /// Offline bundles: one file holding a signed release (and runtimes)
    /// for installs and updates without network access.
    Bundle {
        /// The bundle subcommand.
        #[command(subcommand)]
        command: crate::bundle_cmd::BundleCommand,
    },
    /// Import another agent's home: `from-hermes` reads a Hermes Agent home
    /// (never changing it) into Prometheus profiles.
    Migrate {
        /// The migrate subcommand.
        #[command(subcommand)]
        command: crate::migrate_cmd::MigrateCommand,
    },
    /// Remove this install: the binary, its alias, `prometheus.prev` and its
    /// service units; the home only with `--purge` (`--dry-run` lists all).
    Uninstall(crate::uninstall_cmd::UninstallArgs),
    /// Emergency pause: no new turns or scheduled jobs on any profile of
    /// this home until `pause off` (running turns finish).
    Pause(crate::pause_cmd::PauseArgs),
    /// The Nous Portal subscription and its tool gateway routing.
    Portal {
        /// The portal subcommand.
        #[command(subcommand)]
        command: crate::portal_cmd::PortalCommand,
    },
    /// Run, start, stop and inspect the gateway daemon that owns this
    /// home's sessions (one per home; clients attach to it).
    #[cfg(feature = "gateway")]
    Daemon {
        /// The daemon subcommand.
        #[command(subcommand)]
        command: crate::daemon_cmd::DaemonCommand,
    },
    /// Open the web dashboard: start the daemon's dashboard listener and
    /// sign the browser in with a one-time login link.
    #[cfg(feature = "dashboard")]
    Dashboard(crate::dashboard_cmd::DashboardArgs),
    /// Run the gateway daemon in the foreground with the dashboard listener
    /// open and no browser (for service managers); remote clients attach
    /// through the dashboard listener.
    #[cfg(feature = "dashboard")]
    Serve(ServeArgs),
    /// Open the desktop workspace in the browser (`webapp.enabled`).
    #[cfg(feature = "dashboard")]
    Webapp(crate::webapp_cmd::WebappArgs),
    /// Messaging adapters of the running daemon and the profile's delivery
    /// outbox.
    #[cfg(feature = "gateway")]
    Gateway {
        /// The gateway subcommand.
        #[command(subcommand)]
        command: crate::daemon_cmd::GatewayCommand,
    },
    /// Approve, deny, list and revoke messaging senders' pairing requests.
    /// A code is approved with `prometheus pairing approve <platform>
    /// <code>`, the RPC `pairing.approve` from a client attached to the
    /// profile, or the admin-only chat command `/pair approve <code>`.
    #[cfg(feature = "gateway")]
    Pairing {
        /// The pairing subcommand.
        #[command(subcommand)]
        command: crate::daemon_cmd::PairingCommand,
    },
    /// Hosted app connectors (Gmail, Notion, Linear...) on the Nous
    /// connector gateway: list them, list and rename accounts, connect and
    /// disconnect accounts.
    Connectors {
        /// The connectors subcommand.
        #[command(subcommand)]
        command: crate::connectors_cmd::ConnectorsCommand,
    },
    /// The python bridge: sidecar status, logs and restarts, a doctor for
    /// the managed runtime and plugin environments, and named side
    /// environments for skill scripts.
    Bridge {
        /// The bridge subcommand.
        #[command(subcommand)]
        command: crate::bridge_cmd::BridgeCommand,
    },
    /// MCP servers: add (from a preset or the catalog), list, remove, test
    /// and sign in to the profile's servers, pick their tools, show their
    /// state, and serve Prometheus tools and skills to MCP clients. With no
    /// subcommand on a terminal, an interactive picker.
    #[cfg(feature = "mcp")]
    Mcp {
        /// The mcp subcommand.
        #[command(subcommand)]
        command: Option<crate::mcp_cmd::McpCommand>,
    },
    /// Plugins: list, enable and disable them, manage their capability
    /// grants, and run the adapter conformance kit (`plugins test-adapter`).
    Plugins {
        /// The plugins subcommand.
        #[command(subcommand)]
        command: crate::plugins_cmd::PluginsCommand,
    },
    /// The full-screen terminal UI, attached to the home's gateway daemon
    /// (started when it is not running), or with `--standalone` to an
    /// in-process gateway that serves only this TUI.
    #[cfg(feature = "tui")]
    Tui(crate::tui_cmd::TuiArgs),
    /// Print the JSON Schema of a published contract (`subagent-result`).
    Schema {
        /// The contract.
        #[command(subcommand)]
        command: crate::schema_cmd::SchemaCommand,
    },
    /// The daemon's client protocol: `rpc schema` prints the schema this
    /// binary speaks.
    Rpc {
        /// The rpc subcommand.
        #[command(subcommand)]
        command: crate::rpc_cmd::RpcCommand,
    },
    /// Print a shell completion script (load it from your shell's startup
    /// file, for example `source <(prometheus completions zsh)`).
    Completions {
        /// The shell.
        #[arg(value_enum)]
        shell: crate::completions::Shell,
    },
    /// The completion candidates for a command line (called by the scripts
    /// `completions` prints; reads local files only).
    #[command(name = "__complete", hide = true)]
    Complete {
        /// The shell asking.
        #[arg(value_enum)]
        shell: crate::completions::Shell,
        /// The word under the cursor; without it, the last word.
        #[arg(long, allow_hyphen_values = true)]
        current: Option<String>,
        /// The command line before the cursor, binary name first.
        #[arg(last = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
    /// Stamp the install receipt beside this binary for a package channel
    /// (run by the Homebrew formula, Dockerfile, Nix derivation and Termux
    /// package; no home is touched).
    #[command(name = "install-receipt", hide = true)]
    InstallReceipt(crate::install_receipt_cmd::InstallReceiptArgs),
    /// Check this build against the home before an update installs it
    /// (run by `prometheus update` on the staged binary).
    #[command(hide = true)]
    SelfTest,
    /// Run the bundled `smoke` eval suite with this binary on the mock
    /// provider, offline, in a scratch directory of its own; exit 1 when a
    /// task misses its grade. CI's per-target smoke run; `prometheus eval
    /// run smoke` scores the same suite from M3.
    #[command(name = "smoke-suite", hide = true)]
    SmokeSuite(SmokeSuiteArgs),
    /// List, install, verify and collect the managed runtimes (Node,
    /// CPython, uv, Chromium) shared by every profile.
    Runtimes {
        /// The runtimes subcommand.
        #[command(subcommand)]
        command: RuntimesCommand,
    },
    // A surface whose cargo feature is off still answers to its name, with
    // `surface '<name>' is not included in this build` (spec:
    // workspace-architecture, Version and build identity).
    /// `tui`, left out of this build (exit 69).
    #[cfg(not(feature = "tui"))]
    #[command(hide = true)]
    Tui(crate::surface::MissingArgs),
    /// `gateway`, left out of this build (exit 69).
    #[cfg(not(feature = "gateway"))]
    #[command(hide = true)]
    Gateway(crate::surface::MissingArgs),
    /// `daemon` (the gateway surface), left out of this build (exit 69).
    #[cfg(not(feature = "gateway"))]
    #[command(hide = true)]
    Daemon(crate::surface::MissingArgs),
    /// `dashboard`, left out of this build (exit 69).
    #[cfg(not(feature = "dashboard"))]
    #[command(hide = true)]
    Dashboard(crate::surface::MissingArgs),
    /// `serve` (the dashboard surface), left out of this build (exit 69).
    #[cfg(not(feature = "dashboard"))]
    #[command(hide = true)]
    Serve(crate::surface::MissingArgs),
    /// `webapp` (the dashboard surface), left out of this build (exit 69).
    #[cfg(not(feature = "dashboard"))]
    #[command(hide = true)]
    Webapp(crate::surface::MissingArgs),
    /// `acp`, left out of this build (exit 69).
    #[cfg(not(feature = "acp"))]
    #[command(hide = true)]
    Acp(crate::surface::MissingArgs),
    /// `mcp`, left out of this build (exit 69).
    #[cfg(not(feature = "mcp"))]
    #[command(hide = true)]
    Mcp(crate::surface::MissingArgs),
    /// `evolve`, left out of this build (exit 69).
    #[cfg(not(feature = "evolve"))]
    #[command(hide = true)]
    Evolve(crate::surface::MissingArgs),
    /// `eval`, left out of this build (exit 69).
    #[cfg(not(feature = "eval"))]
    #[command(hide = true)]
    Eval(crate::surface::MissingArgs),
}

/// `prometheus serve` arguments.
#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    /// Bind the dashboard listener to this address instead of
    /// `dashboard.host` (a non-loopback address needs authentication and
    /// TLS).
    #[arg(long)]
    pub host: Option<String>,
    /// Bind this port instead of `dashboard.port`.
    #[arg(long)]
    pub port: Option<u16>,
}

/// `prometheus demo` flags.
#[derive(Debug, Clone, Args)]
pub struct DemoArgs {
    /// Directory to run in (created when missing; must be empty).
    #[arg(long, value_name = "DIR")]
    #[arg(
        help = "Run in DIR, which must be empty or missing (default: a fresh temporary directory)"
    )]
    pub output: Option<PathBuf>,
    /// Keep the directory afterwards.
    #[arg(long, help = "Keep the demo's home and repository afterwards")]
    pub keep: bool,
}

/// `prometheus smoke-suite` flags.
#[derive(Debug, Clone, Args)]
pub struct SmokeSuiteArgs {
    /// Directory to run in (created when missing; must be empty).
    #[arg(long, value_name = "DIR")]
    #[arg(
        help = "Run in DIR, which must be empty or missing (default: a fresh temporary directory)"
    )]
    pub output: Option<PathBuf>,
    /// Keep the directory afterwards.
    #[arg(long, help = "Keep every task's home and workspace afterwards")]
    pub keep: bool,
    /// TOML appended to every task's profile configuration (the
    /// observability overhead gate measures with it).
    #[arg(long, value_name = "FILE", hide = true)]
    pub extra_config: Option<PathBuf>,
    /// Task ids not to run (listed as skipped).
    #[arg(long, value_name = "ID", hide = true)]
    pub skip: Vec<String>,
}

/// `prometheus update` arguments. The flags are independent switches of
/// one command line, not a state machine.
#[derive(Debug, Clone, clap::Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct UpdateArgs {
    /// Only check: print the newest release and exit 10 when it is newer.
    #[arg(long, conflicts_with_all = ["status", "rollback", "set_channel"])]
    pub check: bool,
    /// Print the install's owner, channel, version and last update; no
    /// network request.
    #[arg(long, conflicts_with_all = ["rollback", "set_channel", "channel"])]
    pub status: bool,
    /// Use this channel (`stable` or `canary`) for this run only.
    #[arg(long, value_name = "CHANNEL", conflicts_with_all = ["rollback", "set_channel"])]
    pub channel: Option<String>,
    /// Persist the install's channel (`stable` or `canary`).
    #[arg(long, value_name = "CHANNEL", conflicts_with = "rollback")]
    pub set_channel: Option<String>,
    /// Install the channel's newest release even when it is older than this
    /// build (for example after leaving canary).
    #[arg(long, conflicts_with = "rollback")]
    pub allow_downgrade: bool,
    /// Put back the binary the last update replaced (`prometheus.prev`).
    #[arg(long)]
    pub rollback: bool,
    /// With `--rollback`: also restore the pre-update snapshots, losing the
    /// sessions created since the update (asks first).
    #[arg(long, requires = "rollback")]
    pub restore_snapshots: bool,
    /// Confirm without asking: `--restore-snapshots`, and restarting a
    /// daemon whose turns are still running (once they finish).
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Show what the update would do (versions, download size, changelog,
    /// migrations per profile, runtime pin changes, daemon restart)
    /// without downloading the binary or changing anything; exits 0 or 10
    /// like `--check`.
    #[arg(long, conflicts_with_all = ["status", "rollback", "set_channel", "check"])]
    pub dry_run: bool,
    /// Update from an offline bundle (`prometheus bundle create`) with no
    /// network access; the install becomes `owner = bundle`.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["status", "rollback", "set_channel", "check", "channel"])]
    pub from_bundle: Option<PathBuf>,
    /// Leave a running daemon on the old build (the receipt lists
    /// `daemon_restart_pending`).
    #[arg(long, conflicts_with_all = ["status", "rollback", "set_channel", "check"])]
    pub no_restart: bool,
    /// Also write a full home archive before the swap
    /// (`update.pre_update_backup = "full"` for this run).
    #[arg(long, conflicts_with_all = ["no_backup", "rollback", "check", "status", "set_channel"])]
    pub backup: bool,
    /// Skip the pre-update backup for this run (a store migration still
    /// gets its pre-migration snapshot).
    #[arg(long, conflicts_with_all = ["rollback", "check", "status", "set_channel"])]
    pub no_backup: bool,
}

/// `prometheus debug` subcommands.
#[derive(Debug, Subcommand)]
pub enum DebugCommand {
    /// A plain-text summary of this install and profile to paste into a
    /// support thread (secret values never appear).
    Summary {
        /// Show the first and last 4 characters of secrets longer than 16
        /// characters.
        #[arg(long)]
        show_key_prefixes: bool,
    },
    /// Write a redacted `tar.zst` of the window's logs, the configuration,
    /// `doctor --json`, `version --json`, crash reports and optionally a
    /// session's trajectory (nothing is uploaded).
    Bundle(crate::debug_cmd::bundle::BundleArgs),
    /// Upload a redacted report (setup summary and recent log records) to
    /// a paste service after an explicit yes, or write it locally.
    Share(crate::debug_cmd::share::ShareArgs),
    /// Delete a share recorded in `logs/shares.json` where the target
    /// supports deletion.
    Delete {
        /// The share's URL.
        url: String,
    },
    /// Ask the running daemon to write a dump of every thread and task to
    /// `logs/task-dump-<UTC>.txt` without stopping it.
    DumpTasks,
}

/// `prometheus approvals` subcommands.
#[derive(Debug, Subcommand)]
pub enum ApprovalsCommand {
    /// Run a command line through the approval gate order (hardline,
    /// self-termination, deny rules, bypass, allowlist, dangerous-pattern
    /// and content detectors) without executing it, asking anyone or saving
    /// anything.
    Test(ApprovalsTestArgs),
    /// Propose allow patterns from the commands you approved recently;
    /// nothing changes without `--apply`.
    Suggest(ApprovalsSuggestArgs),
}

/// Arguments of `approvals suggest`.
#[derive(Debug, Clone, clap::Args)]
pub struct ApprovalsSuggestArgs {
    /// Look at approvals from the last N days.
    #[arg(long, value_name = "N", default_value_t = 90)]
    pub days: u32,
    /// Propose only patterns approved at least N times.
    #[arg(long, value_name = "N", default_value_t = 2)]
    pub min_count: usize,
    /// Show at most N proposals.
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub limit: usize,
    /// Add the numbered proposals to `[approvals].allow` in `grants.toml`.
    #[arg(long, value_name = "N,...", value_delimiter = ',')]
    pub apply: Vec<usize>,
}

/// Arguments of `approvals test`.
#[derive(Debug, Clone, clap::Args)]
pub struct ApprovalsTestArgs {
    /// Execution backend to assume (default: `terminal.backend`); hardline
    /// and deny rules apply on every backend.
    #[arg(long, value_name = "NAME")]
    pub backend: Option<String>,
    /// Approval mode to assume (default: `approvals.mode`).
    #[arg(long, value_enum)]
    pub mode: Option<ApprovalModeArg>,
    /// Context to assume; unattended contexts answer from their typed mode.
    #[arg(long, value_enum, default_value_t = ApprovalContextArg::Interactive)]
    pub context: ApprovalContextArg,
    /// The command line, after `--`.
    #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// `approvals test --mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ApprovalModeArg {
    /// Ask for every dangerous call.
    Manual,
    /// Pre-screen with the guardian model, asking a human when it cannot
    /// decide.
    Smart,
    /// Bypass prompts; hardline and deny rules still block.
    Off,
}

/// `approvals test --context`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ApprovalContextArg {
    /// A human can answer.
    Interactive,
    /// A cron job (`approvals.cron_mode`).
    Cron,
    /// `chat -z` (`approvals.single_query_mode`).
    #[value(name = "single_query")]
    SingleQuery,
    /// Webhook/API surfaces (`approvals.unattended_mode`).
    Unattended,
}

/// `prometheus logs` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct LogsArgs {
    /// Component to show (default `agent`), or `list` for every component
    /// and its file.
    #[arg(value_parser = component_values())]
    pub component: Option<String>,
    /// Show the last N matching records; 0 shows all.
    #[arg(short = 'n', long = "lines", value_name = "N", default_value_t = 50)]
    pub lines: usize,
    /// Keep printing new records until interrupted (exit 130).
    #[arg(short, long)]
    pub follow: bool,
    /// Least severe level to show.
    #[arg(long, value_enum)]
    pub level: Option<LogLevelArg>,
    /// Records since `<N>[smhd]` ago or an RFC 3339 time.
    #[arg(long, value_name = "S")]
    pub since: Option<String>,
    /// Only records of this session.
    #[arg(long, value_name = "ID", conflicts_with = "root")]
    pub session: Option<String>,
    /// Only records of the sessions of this root lineage.
    #[arg(long, value_name = "ID")]
    pub root: Option<String>,
}

/// `prometheus usage` arguments.
#[derive(Debug, Clone, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct UsageArgs {
    /// `usage insights`.
    #[command(subcommand)]
    pub command: Option<UsageCommand>,
    /// Calls since `<N>[smhd]` ago or an RFC 3339 time.
    #[arg(long, value_name = "S", default_value = "30d")]
    pub since: String,
    /// Calls before `<N>[smhd]` ago or an RFC 3339 time (default: now).
    #[arg(long, value_name = "S")]
    pub until: Option<String>,
    /// One row per group.
    #[arg(long, value_enum, default_value_t = UsageBy::Day)]
    pub by: UsageBy,
    /// Only this session and its descendants (a root: its whole lineage).
    #[arg(long, value_name = "ID", conflicts_with = "all_profiles")]
    pub session: Option<String>,
    /// Every profile, each through its own scope, with a grand total.
    #[arg(long)]
    pub all_profiles: bool,
}

/// `prometheus usage` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum UsageCommand {
    /// What the profile did: sessions, messages, turns, tools, tokens,
    /// cost, breakdowns by model, provider and source, top tools, skills,
    /// activity by weekday and hour, and the costliest lineages (no model
    /// call).
    Insights(InsightsArgs),
}

/// `prometheus usage insights` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct InsightsArgs {
    /// The last N days.
    #[arg(long, value_name = "N", default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=3650))]
    pub days: u32,
    /// Only lineages started from this surface or platform (`cli`, `tui`,
    /// `cron`, `telegram`...).
    #[arg(long, value_name = "S")]
    pub source: Option<String>,
    /// Every profile, each through its own scope.
    #[arg(long)]
    pub all_profiles: bool,
}

/// `usage --by` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum UsageBy {
    /// Root session of the lineage.
    Root,
    /// Session that made the call.
    Session,
    /// Provider and model.
    Model,
    /// Provider.
    Provider,
    /// Purpose (`main`, `auxiliary.title`, `compression`...).
    Purpose,
    /// Calendar day in the profile's time zone.
    Day,
    /// Profile.
    Profile,
    /// Billing source (`subscription:<provider>/<label>`,
    /// `api_key:<provider>/<label>`, `local`...).
    Billing,
}

fn component_values() -> clap::builder::PossibleValuesParser {
    let mut values: Vec<&'static str> = pmt_obs::logs::COMPONENTS.to_vec();
    values.push("list");
    clap::builder::PossibleValuesParser::new(values)
}

/// `--level` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogLevelArg {
    /// Errors only.
    Error,
    /// Warnings and errors.
    Warn,
    /// Informational and above.
    Info,
    /// Debug and above.
    Debug,
    /// Everything.
    Trace,
}

impl LogLevelArg {
    /// The record level name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// `prometheus prompt` subcommands.
#[derive(Debug, Subcommand)]
pub enum PromptCommand {
    /// Bytes and estimated tokens per prompt layer and per toolset, built
    /// as a new session would build them (no model call, no credentials).
    Size(PromptSizeArgs),
}

/// `prometheus prompt size` options.
#[derive(Debug, clap::Args)]
pub struct PromptSizeArgs {
    /// Surface a new session starts on (`cli`, `cron`, `api`, a messaging
    /// platform such as `telegram` or `webhook`...): its platform hint and
    /// starting toolsets.
    #[arg(long, value_name = "SURFACE", default_value = "cli")]
    pub platform: String,
    /// Comma-separated toolsets (default: the surface's starting toolsets).
    #[arg(long, value_name = "A,B", value_delimiter = ',')]
    pub toolsets: Vec<String>,
    /// `<provider>/<model>` or a model of the configured provider (default:
    /// `model.default`).
    #[arg(long, value_name = "ID")]
    pub model: Option<String>,
}

/// `prometheus trajectory` subcommands.
#[derive(Debug, Subcommand)]
pub enum TrajectoryCommand {
    /// Render a session's trajectory as canonical text or its JSONL lines.
    Show {
        /// Session id, id prefix or title.
        session: String,
        /// Output format.
        #[arg(long, value_enum, default_value_t = TrajectoryFormat::Text)]
        format: TrajectoryFormat,
        /// Clip tool arguments at N characters.
        #[arg(long, value_name = "N", default_value_t = pmt_obs::trajectory::render::ARGS_TRUNC)]
        args_trunc: usize,
        /// Clip each observation at N characters (at least 10000).
        #[arg(long, value_name = "N", default_value_t = pmt_obs::trajectory::render::OBS_TRUNC)]
        obs_trunc: usize,
        /// Clip the whole output at N characters (default: unlimited).
        #[arg(long, value_name = "N")]
        max_chars: Option<usize>,
    },
    /// Write sessions as `ShareGPT` conversations (or their events), one
    /// JSON line per session, redacted; sessions that ended in an error
    /// are left out unless `--include-failed`.
    Export(crate::trajectory_export::ExportArgs),
}

/// `trajectory show --format` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TrajectoryFormat {
    /// Canonical text with [USER], [ACTION], [OBSERVATION]... markers.
    Text,
    /// The stored JSONL lines.
    Jsonl,
}

/// `prometheus grants` subcommands.
#[derive(Debug, Subcommand)]
pub enum GrantsCommand {
    /// List grants (id, subject, capability, surface, time) and approved command patterns.
    List {
        /// Only grants of this subject (`plugin:<id>@<digest>`, `mcp:<server>`, `skill:<name>`, `profile`).
        #[arg(long, value_name = "SUBJECT")]
        subject: Option<String>,
    },
    /// Grant a capability for good (for example `skill:gcloud-ops credential_files`).
    Add {
        /// The holder (`skill:<name>`, `mcp:<server>`, `plugin:<id>@<digest>`, `profile`).
        subject: String,
        /// The capability (`credential_files`, `secrets.read:<NAME>`, ...).
        capability: String,
    },
    /// Remove a grant; running sessions lose it at their next tool call.
    Revoke {
        /// The grant id (`grant_<ULID>`) shown by `grants list`.
        id: String,
    },
}

/// `prometheus security` subcommands.
#[derive(Debug, Subcommand)]
pub enum SecurityCommand {
    /// The security audit log (`<profile>/logs/security-audit.jsonl`).
    Audit {
        /// The audit subcommand.
        #[command(subcommand)]
        command: AuditCommand,
    },
}

/// `prometheus security audit` subcommands.
#[derive(Debug, Subcommand)]
pub enum AuditCommand {
    /// Check the hash chain; exit 1 naming the first record that was edited, inserted or removed.
    Verify,
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
        /// Copy everything from the source profile (--clone-from, else the
        /// selected profile) except sessions, state.db, logs, snapshots and
        /// backups.
        #[arg(long)]
        clone_all: bool,
        /// With a clone, also copy the secrets.
        #[arg(long)]
        clone_secrets: bool,
        /// With a clone, also copy the messaging adapter configuration and
        /// pairing data (left out by default so two profiles never answer
        /// the same bot account).
        #[arg(long)]
        clone_channels: bool,
        /// With a clone, also copy the import sync registry.
        #[arg(long)]
        sync_imports: bool,
        /// Start without bundled skills (`skills.include_bundled = false`); the essential skill stays.
        #[arg(long)]
        no_skills: bool,
        /// A description shown by `profile show` and `profile list`.
        #[arg(long, value_name = "TEXT")]
        description: Option<String>,
        /// Do not write the alias launcher `<name>` (see `profile alias`).
        #[arg(long)]
        no_alias: bool,
    },
    /// Write a launcher that runs `prometheus -p <name> "$@"` into your bin
    /// directory (~/.local/bin, or %LOCALAPPDATA%\Prometheus\bin).
    Alias {
        /// Profile name.
        name: String,
        /// The launcher's name (default: the profile name).
        #[arg(long = "as", value_name = "ALIAS")]
        alias: Option<String>,
    },
    /// Export a profile as a .tar.zst archive (no secrets, sessions or logs
    /// unless asked).
    Export {
        /// Profile name.
        name: String,
        /// The archive to write.
        #[arg(short, long, value_name = "FILE")]
        output: std::path::PathBuf,
        /// Also include the sessions (state.db and its blobs).
        #[arg(long)]
        with_sessions: bool,
        /// Also include the secrets, encrypted with a passphrase read at a
        /// hidden prompt (or from stdin).
        #[arg(long)]
        with_secrets: bool,
    },
    /// Create a profile from an archive `profile export` wrote.
    Import {
        /// The archive.
        file: std::path::PathBuf,
        /// The new profile's name (default: the exported profile's).
        #[arg(long = "as", value_name = "NAME")]
        name: Option<String>,
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

/// The clap command tree, for generated documentation.
#[must_use]
pub fn command() -> clap::Command {
    Cli::command()
}
