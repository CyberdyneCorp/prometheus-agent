//! The `prometheus` binary (alias `pmt`) as a library: the command-line
//! definition, the build identity, the machine-readable output contract and
//! the process startup steps.
//!
//! It lives in a library so `cargo xtask gen` renders the CLI reference from
//! the exact parser the binary uses, and so tests can drive each step.

pub mod build_info;
pub mod cli;
pub mod output;
pub mod startup;
pub mod surface;
pub mod version;

pub use cli::{Cli, Command, command};
