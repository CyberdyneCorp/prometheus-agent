//! Per-profile SQLite session storage (`state.db`), migrations, full-text
//! search and the content-addressed blob store.
//!
//! The store itself is populated in M1 group 6; every entry point takes a
//! [`pmt_config::ProfileScope`] through [`StoreLocation`].

pub mod location;

pub use location::StoreLocation;

/// The newest `state.db` schema version this build reads and writes
/// (`store_meta.schema_version`); every embedded migration bumps it, and a
/// newer store opens read-only. Reported by `prometheus version --json`.
pub const SCHEMA_VERSION: u32 = 1;
