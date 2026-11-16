//! Harness evolution (spec: harness-evolution): what may be in a harness
//! and how a tree is validated ([`harness`]), the opt-in gate
//! ([`gate`]), evolution jobs: their budget arithmetic, task capture
//! and the triggers that queue them ([`job`]), the engine that runs them
//! ([`engine`]), the proposals they produce ([`proposal`]) and the
//! experience library adopted proposals feed ([`experience`]).

pub mod engine;
pub mod experience;
pub mod gate;
pub mod harness;
pub mod job;
pub mod proposal;

/// Whether `id` is `<prefix><ULID>`: the shape of every job and proposal
/// id this crate mints. Ids come from clients (RPC, CLI) and are joined
/// into paths, so anything else (`../`, separators) is refused as
/// not found before it reaches the filesystem.
#[must_use]
pub fn is_minted_id(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix)
        .is_some_and(|rest| ulid::Ulid::from_string(rest).is_ok())
}
