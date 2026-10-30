//! Observability: the trajectory grammar and writer, structured logs, the
//! usage ledger and the budget engine.
//!
//! The grammar and writers are populated in M1 group 7; [`trajectory`]
//! holds the line shape every producer (writer, `sessions export --format
//! trace`, stream-json) shares.

pub mod trajectory;

/// Version of the trajectory event grammar this build writes
/// (`grammar_version`); readers skip event types they do not know.
/// Reported by `prometheus version --json` and recorded in replay exports.
pub const GRAMMAR_VERSION: u32 = 1;
