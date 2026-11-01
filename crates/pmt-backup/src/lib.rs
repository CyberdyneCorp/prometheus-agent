//! Whole-home backups, home restore and per-profile state snapshots (spec:
//! configuration-and-profiles, Whole-home backup, Home restore, State
//! snapshots).
//!
//! - [`create`] writes a `.tar.zst` of the home with a hashed manifest;
//!   [`restore`] verifies a whole archive before changing anything.
//! - [`snapshot`] keeps the critical state of one profile under
//!   `<profile>/snapshots/`.
//! - [`members`] decides what a backup or snapshot holds; [`sqlite`]
//!   copies live databases with SQLite's online backup API.

mod apply;
pub mod create;
mod fsutil;
pub mod manifest;
pub mod members;
pub mod restore;
pub mod snapshot;
pub mod sqlite;

pub use create::{CreateOptions, Created, create};
pub use snapshot::{Reason, SnapshotInfo};
