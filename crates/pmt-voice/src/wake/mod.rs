//! The on-device wake word (spec: voice-mode, Wake word listener; Wake
//! engines and false-trigger control; Single wake listener ownership
//! across surfaces; Wake capture location for remote clients; Wake word
//! profile routing; Wake and voice diagnostics).
//!
//! An armed listener feeds the microphone (or frames a client sends) to
//! an [`engine`] ([`worker`] processes for the real engines, [`fixture`]
//! for recorded audio) and, on the wake phrase, records and answers one
//! request ([`listener`]). [`owner`] keeps one listener per machine;
//! [`command`] is `/wake` and its report.

pub mod command;
pub mod engine;
pub mod fixture;
pub mod listener;
pub mod owner;
pub mod worker;
