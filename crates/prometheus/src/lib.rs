//! The command-line definition of the `prometheus` binary (alias `pmt`).
//!
//! It lives in a library so `cargo xtask gen` renders the CLI reference from
//! the exact parser the binary uses.

use clap::{CommandFactory, Parser};

/// Text after the program name in the identity line
/// `prometheus <semver> (<target>, <commit>)`.
///
/// `prometheus version [--json]` with the full build identity is expanded in
/// M1 group 4 (task 4.5).
const IDENTITY: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("PMT_BUILD_TARGET"),
    ", ",
    env!("PMT_BUILD_COMMIT"),
    ")"
);

// The command name is fixed rather than taken from `argv[0]`, so the `pmt`
// alias prints exactly the same identity line as `prometheus`. `long_about`
// is cleared so this type's doc comment never leaks into `--help`.
/// Parsed command line.
#[derive(Debug, Parser)]
#[command(name = "prometheus", bin_name = "prometheus", version = IDENTITY)]
#[command(about = "Prometheus Agent", long_about = None)]
pub struct Cli {}

/// The clap command tree, for generated documentation.
#[must_use]
pub fn command() -> clap::Command {
    Cli::command()
}
