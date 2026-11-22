//! Pets and achievements (spec: pets): presentational extras of the
//! interactive surfaces that never touch a session's prompt, tools,
//! memory or token use.
//!
//! - [`atlas`]: the petdex sprite atlas and its validation (untrusted
//!   images are checked before anything is written or drawn);
//! - [`store`]: installed pets under `<profile>/pets/<slug>/`;
//! - [`gallery`]: the public gallery's manifest and atlas downloads;
//! - [`state`]: the one activity-to-state table every surface uses;
//! - [`affection`]: the local lexicon behind the heart reaction;
//! - [`size`]: `pets.scale` clamping and terminal widths;
//! - [`frames`] and [`render`]: animation frames and the terminal
//!   encoders (kitty, iTerm2, sixel, half-blocks);
//! - [`hatch`]: generating a pet from a description (drafts, then one
//!   animation row per state packed into an atlas);
//! - [`achievements`]: tiered badges computed from session history.

pub mod achievements;
pub mod affection;
pub mod atlas;
pub mod frames;
pub mod gallery;
pub mod hatch;
pub mod render;
pub mod size;
pub mod state;
pub mod store;

pub use atlas::{Atlas, AtlasError};
pub use state::{Activity, PetState};
pub use store::{Installed, PetMeta, Pets};
