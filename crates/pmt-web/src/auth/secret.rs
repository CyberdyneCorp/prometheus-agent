//! Random credentials and their hashes: only a hash of a login ticket or
//! session token is kept, so a memory dump of the daemon replays nothing.

use sha2::{Digest as _, Sha256};

/// `bytes` random bytes as lowercase hex.
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0_u8; bytes];
    // The OS generator failing is not recoverable for a credential.
    getrandom::fill(&mut buf).expect("the OS random generator is available");
    hex(&buf)
}

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// The SHA-256 of `secret`, as the key it is kept under.
pub(crate) fn hash(secret: &str) -> String {
    hex(&Sha256::digest(secret.as_bytes()))
}

/// Compares two secrets in time independent of where they differ (their
/// lengths are not secret: every token has a fixed length).
pub(crate) fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0_u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}
