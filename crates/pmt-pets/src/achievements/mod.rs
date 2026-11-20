//! Achievements (spec: pets, Achievements from session history;
//! Achievement unlocks celebrate): tiered badges computed on this machine
//! from the profile's session history. Nothing here leaves the machine.
//!
//! - [`catalog`]: the badges and what each counts;
//! - [`stats`]: one session's numbers;
//! - [`scan`]: the incremental scan (only new or changed sessions are
//!   analysed), the cache, the unlock state in `<profile>/achievements.json`
//!   and the snapshot surfaces show.

pub mod catalog;
pub mod scan;
pub mod stats;

pub use scan::{Badge, Profile, ScanOutcome, Snapshot, Unlock};
