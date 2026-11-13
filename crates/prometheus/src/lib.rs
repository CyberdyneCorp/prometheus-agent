//! The `prometheus` binary (alias `pmt`) as a library: the command-line
//! definition, the build identity, the machine-readable output contract and
//! the process startup steps.
//!
//! It lives in a library so `cargo xtask gen` renders the CLI reference from
//! the exact parser the binary uses, and so tests can drive each step.

pub mod accounts;
pub mod advisories;
pub mod approvals_cmd;
pub mod auth_cmd;
pub mod automation;
pub mod backup_cmd;
pub mod bridge_cmd;
pub mod browser_cmd;
pub mod build_info;
pub mod bundle_cmd;
pub mod cache_cmd;
pub mod chat;
pub mod cli;
pub mod completions;
pub mod config_cmd;
pub mod connectors_cmd;
pub mod connectors_host;
pub mod console_cmd;
pub mod context;
pub mod context_engine;
pub mod cron_cmd;
pub mod cron_host;
pub mod curator_cmd;
pub mod daemon_client;
#[cfg(feature = "gateway")]
pub mod daemon_cmd;
#[cfg(feature = "dashboard")]
pub mod dashboard_cmd;
pub mod debug_cmd;
pub mod demo_cmd;
pub mod doctor_cmd;
pub mod egress;
pub mod fallback_cmd;
pub mod files_cmd;
pub mod grants_cmd;
pub mod hooks_host;
pub mod init;
pub mod insights_cmd;
pub mod install_receipt_cmd;
pub mod journey_cmd;
pub mod local_cmd;
pub mod logs_cmd;
pub mod lsp_cmd;
#[cfg(feature = "mcp")]
pub mod mcp_cmd;
pub mod mcp_host;
pub mod memory_cmd;
pub mod memory_provider;
pub mod migrate_cmd;
pub mod moa_cmd;
pub mod model_cmd;
pub mod model_slash;
pub mod output;
pub mod pause_cmd;
pub mod plugins_cmd;
pub mod plugins_host;
pub mod portal_cmd;
pub mod profile_cmd;
pub mod projects_cmd;
pub mod prompt;
pub mod prompt_cmd;
pub mod providers_cmd;
pub mod replay;
pub mod replay_cmd;
pub mod rpc_cmd;
pub mod runtimes_cmd;
pub mod schema_cmd;
pub mod secrets_cmd;
pub mod security_cmd;
pub mod self_test_cmd;
pub mod session_export;
pub mod session_import;
pub mod sessions_browse;
pub mod sessions_cmd;
pub mod setup_cmd;
pub mod skills_cmd;
pub mod slash;
pub mod slash_handlers;
pub mod smoke_suite;
pub mod startup;
pub mod status_cmd;
pub mod surface;
pub mod themes_cmd;
pub mod tools_cmd;
pub mod trajectory_cmd;
pub mod trajectory_export;
#[cfg(feature = "tui")]
pub mod tui_cmd;
pub mod uninstall_cmd;
pub mod update_cmd;
pub mod usage_cmd;
pub mod vault_cmd;
pub mod verify_cmd;
pub mod version;
#[cfg(feature = "dashboard")]
pub mod webapp_cmd;
pub mod webhooks_cmd;
pub mod worktrees_cmd;

pub use cli::{
    ApprovalsCommand, AuthCommand, BackupCommand, Cli, Command, ConfigCommand, DemoArgs,
    GrantsCommand, LogsArgs, ProfileCommand, SecretsCommand, SessionsCommand, SkillsCommand,
    SnapshotsCommand, ToolsCommand, ToolsetsCommand, TrajectoryCommand, UpdateArgs, UsageArgs,
    command,
};
pub use context::Context;
