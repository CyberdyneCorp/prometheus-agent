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
//! - [`search`] full-text search over derived indexes that fail open,
//!   [`integrity`] the startup check and quarantine, [`rewind`] soft
//!   deletes, [`delete`] and [`retention`] session removal, [`usage`] the
//!   usage ledger, [`listing`] listings, pickers and size guards, and
//!   [`credential_state`] provider credential request counts and benches,
//!   and [`tool_surface`] the tool surface each session froze.

pub mod blobs;
pub mod compaction;
mod contention;
pub mod credential_state;
pub mod delete;
mod error;
mod files;
mod index;
pub mod integrity;
pub mod leases;
pub mod listing;
pub mod location;
pub mod messages;
mod migrate;
pub mod model;
pub mod model_context;
pub mod retention;
pub mod rewind;
pub mod search;
pub mod sessions;
mod store;
pub mod titles;
pub mod tool_surface;
pub mod trajectories;
pub mod usage;
pub mod usage_query;

pub use blobs::{BlobRef, GcReport};
pub use compaction::{Compacted, Compaction};
pub use contention::WriteClass;
pub use delete::Deleted;
pub use index::DOCTOR_GROUP;
pub use integrity::{QuarantineReport, quarantine_reports};
pub use leases::{Acquire, Holder, Lease, LeaseKind};
pub use listing::ListOptions;
pub use location::StoreLocation;
pub use messages::{Message, NewMessage, Visibility};
pub use model::{Archived, CostStatus, EndReason, LineageKind, Origin, Role, Source, TitleSource};
pub use retention::Maintenance;
pub use rewind::Rewound;
pub use search::{SearchHit, SearchMode, SearchOptions, SearchResults, SearchSort};
pub use sessions::{
    HIDDEN_SOURCE_TAGS, NewSession, ONESHOT_TAG, Session, TreeNode, check_source_tag,
};
pub use store::{CHECKPOINT_EVERY, MAX_READERS, Store};
pub use titles::TitleWrite;
pub use usage::{Tokens, UsageEvent, UsageScope, UsageTotals};
pub use usage_query::UsageFilter;

/// The newest `state.db` schema version this build reads and writes
/// (`store_meta.schema_version`); every embedded migration bumps it, and a
/// newer store opens read-only. Reported by `prometheus version --json`.
pub const SCHEMA_VERSION: u32 = 6;

#[cfg(test)]
mod tests {
    #[test]
    fn schema_version_is_the_last_migration() {
        let last = super::migrate::MIGRATIONS.last().unwrap();
        assert_eq!(last.version, super::SCHEMA_VERSION);
    }
}
