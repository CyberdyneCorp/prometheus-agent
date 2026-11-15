//! Harness evolution (spec: harness-evolution): what may be in a harness
//! and how a tree is validated ([`harness`]), the opt-in gate
//! ([`gate`]), evolution jobs: their budget arithmetic, task capture
//! and the triggers that queue them ([`job`]), the engine that runs them
//! ([`engine`]) and the proposals they produce ([`proposal`]).

pub mod engine;
pub mod gate;
pub mod harness;
pub mod job;
pub mod proposal;
