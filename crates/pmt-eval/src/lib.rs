//! Replay and evaluation (spec: replay-and-evaluation): the `.pmtrec`
//! session [`recording`] format with its integrity check, and the
//! [`compare`]-ison of a replay with what it replays (divergence diffs,
//! trajectories without ids and timestamps). The replay itself runs in
//! the binary, where sessions are built; `prometheus eval` (answer-free
//! suites) arrives in M3.

pub mod compare;
pub mod recording;

pub use recording::{Manifest, Recording};
