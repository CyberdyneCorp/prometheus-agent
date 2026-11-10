//! The skills hub (spec: skills, Hub install pipeline and trust policy;
//! Skills lockfile and tamper detection; Skill signatures and provenance;
//! Hub discovery, inspection, updates and audit; Hub source identifiers;
//! Custom skill taps; Skill publishing and snapshots).
//!
//! - [`ident`] parses the identifier forms; [`sources`] fetches each into
//!   an in-memory [`bundle::Bundle`] over [`http::Http`].
//! - [`install`] is the pipeline: quarantine, scan, signature, trust
//!   decision, move into the profile, record in `skills.lock`.
//! - [`maintain`] checks, updates, audits, uninstalls, accepts and resets
//!   hub installs and reproduces a lockfile; [`search`] browses and
//!   searches; [`inspect`] previews without writing.
//! - [`taps`], [`snapshot`] and [`publish`] manage taps, snapshots and
//!   publishing; [`sign`] and [`keyless`] write and verify `SKILL.sig`.
//!
//! The binary wires it: it builds a [`hub::Hub`] from the profile (HTTP
//! client from `network.*`, `GITHUB_TOKEN` from the secrets store) and
//! records usage rows and blueprint suggestions after an install.

pub mod bundle;
pub mod http;
pub mod hub;
pub mod ident;
pub mod inspect;
pub mod install;
pub mod keyless;
pub mod maintain;
pub mod publish;
pub mod search;
pub mod sign;
pub mod snapshot;
pub mod sources;
pub mod taps;
#[cfg(feature = "testkit")]
pub mod testkit;

pub use hub::Hub;
pub use ident::Identifier;
