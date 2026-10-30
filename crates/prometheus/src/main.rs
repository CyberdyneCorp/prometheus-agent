//! The `prometheus` binary (alias `pmt`). Every surface is a subcommand of
//! this one executable; M0 only provides the build identity.

use clap::Parser;

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

/// The command name is fixed rather than taken from `argv[0]`, so the `pmt`
/// alias prints exactly the same identity line as `prometheus`.
#[derive(Parser)]
#[command(name = "prometheus", bin_name = "prometheus", version = IDENTITY)]
#[command(about = "Prometheus Agent")]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
