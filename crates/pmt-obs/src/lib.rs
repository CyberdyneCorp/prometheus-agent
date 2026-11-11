//! Observability: the trajectory grammar and writer, structured logs, the
//! usage ledger and the budget engine.
//!
//! - [`trajectory`]: the one JSONL grammar every surface records, its
//!   canonical text rendering, the redacted writer and retention;
//! - [`logs`]: structured JSON logs (the `tracing` subscriber) and the
//!   reading side of `prometheus logs`;
//! - [`usage`]: pricing, the ledger writer and grouped reports;
//! - [`budget`]: pre-call budget checks, warnings and the agent-write
//!   guard on `budgets.*`;
//! - [`health`]: the component health model;
//! - [`sharegpt`]: sessions as `ShareGPT` training records;
//! - [`taskdump`]: the open spans of the process, for task dumps;
//! - [`telemetry`]: the opt-in telemetry gate (nothing is sent by default);
//! - [`metrics`]: the metrics catalog (process-wide registry, Prometheus
//!   text rendering);
//! - [`otel`]: opt-in OpenTelemetry export, the span model and trace
//!   context;
//! - [`recording`]: the per-session journal session recordings are built
//!   from (prompt builds, model calls, tool results).

pub mod budget;
pub mod health;
pub mod logs;
pub mod metrics;
pub mod otel;
pub mod recording;
pub mod redaction;
pub mod sharegpt;
pub mod taskdump;
pub mod telemetry;
pub mod time;
pub mod trajectory;
pub mod usage;

/// Major version of the trajectory event grammar this build writes
/// (`grammar_version`); readers skip event types they do not know and
/// refuse a newer major. Reported by `prometheus version --json` and
/// recorded in replay exports.
pub const GRAMMAR_VERSION: u32 = 1;
