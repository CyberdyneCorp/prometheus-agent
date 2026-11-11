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
//!   [`tool_surface`] the tool surface each session froze, and
//!   [`compression_state`] each session's automatic-compaction guards,
//!   and [`memory`] the built-in memory entries and staged changes, and
//!   [`gateway`] the messaging gateway's routes, pairing and outbox, and
//!   [`mcp_spend`] what conversations spent on costed MCP tools, and
//!   [`process_receipts`] what ended background processes left behind, and
//!   [`verification`] `prometheus verify` evidence, and [`cron`] scheduled
//!   jobs, their runs, delivery outbox and tick lease.

pub mod automation;
pub mod blobs;
pub mod compaction;
pub mod compression_state;
mod contention;
pub mod credential_state;
pub mod cron;
pub mod delegations;
pub mod delete;
mod error;
mod files;
pub mod fork;
pub mod gateway;
mod index;
pub mod insights;
pub mod inspect;
pub mod integrity;
pub mod journal;
pub mod leases;
pub mod listing;
pub mod location;
pub mod mcp_spend;
pub mod memory;
pub mod memory_index;
pub mod memory_revisions;
pub mod messages;
mod migrate;
pub mod model;
pub mod model_context;
pub mod onboarding;
pub mod pins;
pub mod portable;
pub mod prefill;
pub mod prefs;
pub mod process_receipts;
pub mod prompts;
pub mod provider_state;
pub mod rebind;
pub mod recover;
pub mod retention;
pub mod rewind;
pub mod search;
pub mod sessions;
pub mod skill_ledger;
pub mod skill_usage;
mod store;
pub mod titles;
pub mod todos;
pub mod tool_surface;
pub mod trajectories;
pub mod usage;
pub mod usage_query;
pub mod verification;

pub use automation::AutomationRow;
pub use blobs::{BlobRef, GcReport};
pub use compaction::{Compacted, Compaction};
pub use compression_state::CompressionState;
pub use contention::WriteClass;
pub use delete::Deleted;
pub use index::DOCTOR_GROUP;
pub use insights::{Activity, ActivityRow, LabeledSession};
pub use integrity::{QuarantineReport, quarantine_reports};
pub use journal::{JournalKind, JournalRecord};
pub use leases::{Acquire, Holder, Lease, LeaseKind};
pub use listing::{ListOptions, SessionCounts};
pub use location::StoreLocation;
pub use memory::{MemoryBatch, MemoryEntry, MemoryPending};
pub use memory_revisions::{Author, Operation, Revision, RevisionFilter};
pub use messages::{Message, NewMessage, Visibility};
pub use model::{
    Archived, CostStatus, EndReason, LineageKind, MemoryKind, MemoryOrigin, MemoryStatus,
    MemoryTarget, MemoryTier, Origin, Role, Source, TitleSource,
};
pub use pins::Pin;
pub use prefill::SessionPrefill;
pub use process_receipts::ProcessReceipt;
pub use prompts::{PromptTier, SessionPrompt};
pub use retention::Maintenance;
pub use rewind::Rewound;
pub use search::{SearchHit, SearchMode, SearchOptions, SearchResults, SearchSort};
pub use sessions::{
    HIDDEN_SOURCE_TAGS, NewSession, ONESHOT_TAG, Session, TreeNode, check_source_tag,
};
pub use skill_usage::{CreatedBy, SkillActivity, SkillState, SkillUsage};
pub use store::{CHECKPOINT_EVERY, MAX_READERS, Store};
pub use titles::TitleWrite;
pub use todos::SessionTodos;
pub use usage::{Tokens, UsageEvent, UsageScope, UsageTotals};
pub use usage_query::UsageFilter;

/// The newest `state.db` schema version this build reads and writes
/// (`store_meta.schema_version`); every embedded migration bumps it, and a
/// newer store opens read-only. Reported by `prometheus version --json`.
pub const SCHEMA_VERSION: u32 = 35;

#[cfg(test)]
mod tests {
    #[test]
    fn schema_version_is_the_last_migration() {
        let last = super::migrate::MIGRATIONS.last().unwrap();
        assert_eq!(last.version, super::SCHEMA_VERSION);
    }
}
