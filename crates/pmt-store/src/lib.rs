//! Per-profile SQLite session storage (`state.db`) and the
//! content-addressed blob store (spec: session-storage).
//!
//! - [`Store::open`] is the only way in: it takes a
//!   [`pmt_config::ProfileScope`], binds the file to the profile's id,
//!   applies the embedded migrations and enforces owner-only files.
//! - [`sessions`] holds session records and typed lineage, [`messages`]
//!   the transactional transcript, [`blobs`] the blob store and its
//!   collector, [`leases`] the turn lease and compaction lock,
//!   [`compaction`] in-place compaction and [`titles`] titles and session
//!   references.

pub mod blobs;
pub mod compaction;
mod error;
mod files;
pub mod leases;
pub mod location;
pub mod messages;
mod migrate;
pub mod model;
pub mod sessions;
mod store;
pub mod titles;

pub use blobs::{BlobRef, GcReport};
pub use compaction::{Compacted, Compaction};
pub use leases::{Acquire, Holder, Lease, LeaseKind};
pub use location::StoreLocation;
pub use messages::{Message, NewMessage, Visibility};
pub use model::{Archived, EndReason, LineageKind, Origin, Role, Source, TitleSource};
pub use sessions::{NewSession, Session, TreeNode};
pub use store::{CHECKPOINT_EVERY, MAX_READERS, Store};
pub use titles::TitleWrite;

/// The newest `state.db` schema version this build reads and writes
/// (`store_meta.schema_version`); every embedded migration bumps it, and a
/// newer store opens read-only. Reported by `prometheus version --json`.
pub const SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    #[test]
    fn schema_version_is_the_last_migration() {
        let last = super::migrate::MIGRATIONS.last().unwrap();
        assert_eq!(last.version, super::SCHEMA_VERSION);
    }
}
