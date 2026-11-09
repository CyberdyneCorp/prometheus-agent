//! LSP diagnostics after file edits (spec: lsp-diagnostics).
//!
//! Real language servers run as confined background processes; after
//! `write_file` and `patch` change a file, the diagnostics the edit
//! introduced (and only those) are added to the tool result, or to a later
//! one when the server is slower than the write's small budget. A server
//! never fails or blocks a write beyond that budget.
//!
//! - [`registry`]: built-in and custom servers, their install methods and
//!   whether they execute project code;
//! - [`workspace`]: git-root gating and workspace trust;
//! - [`install`]: binaries on `PATH` or installed under
//!   `<PROMETHEUS_HOME>/runtimes/lsp/`;
//! - [`launch`]: the sandboxed server start;
//! - [`client`], [`protocol`], [`check`]: the JSON-RPC session and one check;
//! - [`pool`]: a session's servers, broken marks and idle shutdown;
//! - [`session`]: [`SessionDiagnostics`], what the file tools and the turn
//!   loop call;
//! - [`status`]: what `prometheus lsp status` and RPC `lsp.status` read.

pub mod check;
pub mod client;
pub mod install;
pub mod launch;
pub mod pool;
pub mod protocol;
pub mod registry;
pub mod session;
pub mod status;
pub mod workspace;

pub use session::{INLINE_BUDGET, LspSetup, Route, SessionDiagnostics, Toolchain};
