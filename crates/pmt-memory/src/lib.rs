//! Built-in memory: typed entries in the profile's `state.db`, a budgeted
//! `core` tier frozen into the system prompt at session start and an
//! `archival` tier reached by per-turn recall (spec: memory).
//!
//! - [`MemorySession`] is one session's memory: it implements
//!   `pmt_agent::tools::MemoryPort` (the `memory` tool and recall), renders
//!   the prompt snapshot and counts the review nudge.
//! - [`ops`] validates tool calls, [`batch`] applies them to one target's
//!   entries in memory, [`write`] commits, stages and approves them through
//!   one store transaction each.
//! - [`snapshot`] and [`recall`] are what the model sees; [`scan`] keeps
//!   planted instructions and secrets out of both.
//! - [`markdown`] and [`manage`] are the user's edit, export and import
//!   view; [`slash`] is `/memory`, [`status`] its status, [`review`] the
//!   background review, [`tool`] the registry entry.
//!
//! External memory providers and the revision journal arrive in M2.

pub mod batch;
pub mod budget;
pub mod id;
pub mod manage;
pub mod markdown;
pub mod ops;
pub mod recall;
pub mod review;
pub mod scan;
pub mod session;
pub mod slash;
pub mod snapshot;
pub mod status;
pub mod tool;
pub mod write;

pub use session::{Approver, MemorySession, MemorySetup};
