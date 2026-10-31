//! The execution environment behind the `terminal` and `process` tools
//! (spec: execution-environments).
//!
//! - [`LocalTerminal`] runs one session's commands on the `local` backend
//!   under the session's OS sandbox ([`pmt_sandbox::Sandbox`]).
//! - [`env::ChildEnv`] builds every child environment from an allowlist.
//! - [`process::ProcessRegistry`] tracks background processes and reaps
//!   them when the session ends.
//! - [`shell`] and [`windows_shell`] pick the shell ([`doctor`] reports
//!   it); [`cwd`] the start
//!   directory; [`guard`] is the fail-closed pre-execution chain;
//!   [`classify`], [`shape`] and [`spill`] plan and shape foreground runs.
//!
//! [`tool`] wraps them as the model-facing `terminal` and `process` tools.

pub mod args;
pub mod classify;
pub mod cwd;
pub mod doctor;
pub mod env;
pub mod guard;
mod local;
pub mod process;
pub mod result;
pub mod shape;
pub mod shell;
pub mod spawn;
pub mod spill;
pub mod tool;
pub mod windows_shell;

pub use args::TerminalArgs;
pub use local::{LocalSetup, LocalTerminal};
pub use result::{Status, TerminalResult};
