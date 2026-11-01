//! `prometheus rpc` (spec: workspace-architecture, Contracts generated from
//! types and checked in): `rpc schema` prints the client-protocol schema of
//! the running binary, the same document as the checked-in
//! `schema/rpc.json`.

use clap::Subcommand;
use pmt_core::error::Error;

/// `prometheus rpc` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum RpcCommand {
    /// Print the client-protocol schema (methods, server requests and
    /// events, each with its owning capability and contract).
    Schema,
}

/// Runs `prometheus rpc <command>`.
///
/// # Errors
///
/// `output.io` when stdout cannot be written.
pub fn run(command: &RpcCommand) -> Result<u8, Error> {
    match command {
        RpcCommand::Schema => {
            let schema = pmt_rpc::Catalog::builtin().schema();
            crate::output::print_raw(&pmt_config::catalog::pretty(&schema))
        }
    }
}
