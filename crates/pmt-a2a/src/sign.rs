//! Push-notification signatures and token checks (spec: agent-interop, A2A
//! push notifications; A2A authentication, trust and rate limits).
//!
//! A push is signed with HMAC-SHA256 over `<timestamp>.<raw body>`, sent as
//! `X-A2A-Signature: sha256=<hex>` beside `X-A2A-Timestamp: <unix
//! seconds>`, so the timestamp a receiver checks for freshness is covered
//! by the signature.

use sha2::{Digest as _, Sha256};

/// The signature header.
pub const SIGNATURE_HEADER: &str = "X-A2A-Signature";
/// The timestamp header.
pub const TIMESTAMP_HEADER: &str = "X-A2A-Timestamp";

/// HMAC-SHA256 (RFC 2104).
#[must_use]
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// Lowercase hex.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// The `X-A2A-Signature` value of `body` sent at `timestamp`.
#[must_use]
pub fn signature(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let mut message = format!("{timestamp}.").into_bytes();
    message.extend_from_slice(body);
    format!("sha256={}", hex(&hmac_sha256(secret, &message)))
}

/// Whether `given` is the signature of `body` at `timestamp`.
#[must_use]
pub fn verify(secret: &[u8], timestamp: i64, body: &[u8], given: &str) -> bool {
    constant_time_eq(
        signature(secret, timestamp, body).as_bytes(),
        given.as_bytes(),
    )
}

/// Compares two byte strings in time independent of where they differ.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = u8::from(a.len() != b.len());
    for (i, x) in a.iter().enumerate() {
        diff |= x ^ b.get(i).copied().unwrap_or(!x);
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn signatures_cover_the_timestamp() {
        let sig = signature(b"k", 100, b"{}");
        assert!(verify(b"k", 100, b"{}", &sig));
        assert!(!verify(b"k", 101, b"{}", &sig));
        assert!(!verify(b"x", 100, b"{}", &sig));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abd", b"abc"));
    }
}
