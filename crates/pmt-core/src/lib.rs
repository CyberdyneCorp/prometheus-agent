//! Shared core types for every Prometheus crate: typed ids (`ses_<ULID>`,
//! `msg_<ULID>`), the error taxonomy and the process exit-code table, the
//! launch environment, closed name registries, the cancellation token tree
//! and panic isolation.

pub mod cancel;
pub mod env;
pub mod error;
pub mod exit;
pub mod id;
pub mod panic;
pub mod registry;
pub mod secret;

pub use error::{Error, ErrorKind, Result};
