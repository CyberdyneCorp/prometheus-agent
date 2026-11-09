//! Voice mode (spec: voice-mode): one pipeline shared by every
//! interactive surface. A recording ([`record`]) runs through two-stage
//! silence detection ([`vad`]); its transcript is filtered
//! ([`filter`]) and either sent as a user message, dropped, or taken as a
//! stop phrase; replies are spoken sentence by sentence while they
//! generate ([`speaker`], [`speakable`]); [`converse`] keeps the
//! push-to-talk loop going hands-free. [`mode`] is `/voice`.
//!
//! Hosts plug in the profile's speech providers and the machine's audio
//! ([`device`] ports; [`system`] for real devices, [`fixture`] for
//! recorded audio).

pub mod converse;
pub mod device;
pub mod filter;
pub mod fixture;
pub mod mode;
pub mod pcm;
pub mod record;
pub mod speakable;
pub mod speaker;
pub mod system;
pub mod vad;
