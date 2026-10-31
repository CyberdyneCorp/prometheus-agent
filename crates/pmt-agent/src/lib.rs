//! The agent turn loop, system-prompt assembly and context compression.
//!
//! [`turn`] runs a session's turns ([`turn::Agent::run_turn`]), [`prompt`]
//! builds and freezes the layered system prompt, [`references`] expands
//! `@` context references, [`tools`] holds the agent-level tools the loop
//! executes itself (`todo`, `clarify`, `session_search`), [`vision`]
//! how images reach the model and [`compress`] context compression.

pub mod compress;
pub mod prompt;
pub mod references;
pub mod tools;
pub mod turn;
pub mod vision;

/// Lower-case hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// `sha256:<hex>` of `bytes`.
pub(crate) fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}
