//! Scheduled jobs (spec: scheduled-jobs).
//!
//! - [`schedule`]: the unambiguous grammar, next-run computation and the
//!   canonical echo.
//! - [`job`]: job definitions, states and status literals, and payload
//!   validation ([`validate`]) with the prompt safety scan ([`scan`]).
//! - [`grace`]: lateness classes and what to do about missed occurrences.
//! - [`scheduler`]: the trigger-independent claim of due occurrences
//!   (at most once), the tick lease and recovery after a crash.
//! - [`exec`]: one run from preflight to delivery, over a [`exec::Runner`]
//!   the binary implements (agent sessions, scripts).
//! - [`deliver`]: response framing, silence and the durable outbox.
//! - [`ops`]: lifecycle operations shared by the CLI and the `cronjob`
//!   tool, [`doctor`] the health report.

pub mod clock;
pub mod deliver;
pub mod doctor;
pub mod duration;
pub mod exec;
pub mod fields;
pub mod grace;
pub mod job;
pub mod ops;
pub mod scan;
pub mod schedule;
pub mod scheduler;
pub mod target;
pub mod validate;
