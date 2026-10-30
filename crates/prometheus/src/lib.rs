//! The `prometheus` binary (alias `pmt`) as a library: the command-line
//! definition, the build identity, the machine-readable output contract and
//! the process startup steps.
//!
//! It lives in a library so `cargo xtask gen` renders the CLI reference from
//! the exact parser the binary uses, and so tests can drive each step.

pub mod backup_cmd;
pub mod build_info;
pub mod cli;
pub mod config_cmd;
pub mod context;
pub mod fallback_cmd;
pub mod grants_cmd;
pub mod logs_cmd;
pub mod model_cmd;
pub mod model_slash;
pub mod output;
pub mod profile_cmd;
pub mod prompt;
pub mod secrets_cmd;
pub mod session_export;
pub mod sessions_cmd;
pub mod slash_handlers;
pub mod startup;
pub mod surface;
pub mod trajectory_cmd;
pub mod usage_cmd;
pub mod version;

pub use cli::{
    BackupCommand, Cli, Command, ConfigCommand, GrantsCommand, LogsArgs, ProfileCommand,
    SecretsCommand, SessionsCommand, SnapshotsCommand, TrajectoryCommand, UsageArgs, command,
};
pub use context::Context;
