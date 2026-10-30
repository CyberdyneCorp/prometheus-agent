//! The `prometheus` binary (alias `pmt`). Every surface is a subcommand of
//! this one executable; M0 only provides the build identity.

use clap::Parser;
use prometheus::Cli;

fn main() {
    let _cli = Cli::parse();
}
