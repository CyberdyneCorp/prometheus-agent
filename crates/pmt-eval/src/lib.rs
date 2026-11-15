//! Replay and evaluation (spec: replay-and-evaluation): the `.pmtrec`
//! session [`recording`] format with its integrity check, the
//! [`compare`]-ison of a replay with what it replays, and the parts of
//! `prometheus eval` that need no session: the [`suite`] format, the
//! [`run`] layout and its resumption, the answer-free [`boundary`],
//! [`grader`]s and post-hoc [`judge`]-ing, [`report`]s with bootstrap
//! comparison, [`bench`]mark adapters, session-to-task [`capture`], batch
//! datasets and statistics ([`batch`]), toolset [`distribution`]s and the
//! trajectory [`compress`]-or. Sessions themselves run in the binary.

pub mod batch;
pub mod bench;
pub mod boundary;
pub mod capture;
pub mod compare;
pub mod compress;
pub mod distribution;
pub mod grader;
pub mod judge;
pub mod recording;
pub mod report;
pub mod rng;
pub mod run;
pub mod suite;

pub use recording::{Manifest, Recording};
