//! Harness evolution (spec: harness-evolution): what may be in a harness
//! and how a tree is validated ([`harness`]), the opt-in gate
//! ([`gate`]), and evolution jobs: their budget arithmetic, task capture
//! and the triggers that queue them ([`job`]).

pub mod gate;
pub mod harness;
pub mod job;
