//! Shared core types for every Prometheus crate: typed ids (`ses_<ULID>`,
//! `msg_<ULID>`), the error taxonomy and the process exit-code table, the
//! launch environment, closed name registries, the cancellation token tree,
//! panic isolation, runtime records and the home writer lock, the open-file
//! limit raise, the doctor check shape and the typed capability vocabulary.

pub mod audit;
pub mod battery;
pub mod cancel;
pub mod capability;
pub mod doctor;
pub mod env;
pub mod error;
pub mod exit;
pub mod hooks;
pub mod human;
pub mod id;
pub mod inventory;
pub mod nofile;
pub mod panic;
pub mod pause;
pub mod pii;
pub mod private_fs;
pub mod protocol;
pub mod provenance;
pub mod registry;
pub mod runtime;
pub mod secret;
pub mod secret_paths;
pub mod subagent_tree;
pub mod tasks;

pub use error::{Error, ErrorKind, Result};
