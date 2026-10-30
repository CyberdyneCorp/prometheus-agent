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
}

/// The clap command tree, for generated documentation.
#[must_use]
pub fn command() -> clap::Command {
    Cli::command()
}
