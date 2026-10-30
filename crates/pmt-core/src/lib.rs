//! Shared core types for every Prometheus crate: ids (`ses_<ULID>`,
//! `msg_<ULID>`), the error taxonomy and the process exit-code table.
//!
//! M0 holds only the tables the generated error catalog is built from; ids
//! and the rest of the taxonomy are expanded in M1 group 4.

pub mod error;
pub mod exit;
