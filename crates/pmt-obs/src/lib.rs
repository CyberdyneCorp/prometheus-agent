//! Observability: the trajectory grammar and writer, structured logs, the
//! usage ledger and the budget engine.
//!
//! - [`trajectory`]: the one JSONL grammar every surface records, its
//!   canonical text rendering, the redacted writer and retention;
//! - [`logs`]: structured JSON logs (the `tracing` subscriber) and the
//!   reading side of `prometheus logs`.

pub mod logs;
pub mod trajectory;

/// Major version of the trajectory event grammar this build writes
/// (`grammar_version`); readers skip event types they do not know and
/// refuse a newer major. Reported by `prometheus version --json` and
/// recorded in replay exports.
pub const GRAMMAR_VERSION: u32 = 1;
