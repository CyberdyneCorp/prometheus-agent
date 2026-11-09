//! `prometheus schema <contract>`: the JSON Schema of a published
//! contract this binary writes (spec: subagent-delegation, Structured
//! result contract).

use clap::Subcommand;
use pmt_core::error::Error;

/// The contracts `prometheus schema` prints.
#[derive(Debug, Clone, Subcommand)]
pub enum SchemaCommand {
    /// The `SubagentResult` a delegated child returns (contract version 1).
    SubagentResult,
}

/// Prints the schema of `command`.
///
/// # Errors
///
/// `output.io` when stdout cannot be written.
pub fn run(command: &SchemaCommand) -> Result<u8, Error> {
    let schema = match command {
        SchemaCommand::SubagentResult => pmt_delegation::result::schema(),
    };
    crate::output::print_raw(&pmt_config::catalog::pretty(&schema))
}
