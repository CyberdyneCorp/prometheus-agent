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
