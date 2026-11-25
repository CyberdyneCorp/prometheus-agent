//! Request signatures: HMAC-SHA1/SHA256, SHA-256, constant-time
//! comparison and the encodings platforms sign in.

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Digest as _;

/// HMAC-SHA256 of `data` with `key`.
#[must_use]
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// HMAC-SHA1 of `data` with `key`.
#[must_use]
pub fn hmac_sha1(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// SHA-1 of `data` (signatures some platforms still make with it).
#[must_use]
pub fn sha1(data: &[u8]) -> Vec<u8> {
    use sha1::Digest as _;
    sha1::Sha1::digest(data).to_vec()
}

/// SHA-256 of `data`.
#[must_use]
pub fn sha256(data: &[u8]) -> Vec<u8> {
    sha2::Sha256::digest(data).to_vec()
}

/// Whether `a` and `b` are equal, in time independent of where they
/// differ.
#[must_use]
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Decodes standard base64.
///
/// # Errors
///
/// Text that is not base64.
pub fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|e| e.to_string())
}

/// Standard base64.
#[must_use]
pub fn base64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Decodes standard base64.
#[must_use]
pub fn unbase64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()
}

/// Lower-case hex.
#[must_use]
pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // RFC 4231 test case 2.
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 2202 test case 2.
        assert_eq!(
            hex(&hmac_sha1(b"Jefe", b"what do ya want for nothing?")),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
        assert!(same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"a", b"ab"));
        assert_eq!(unbase64(&base64(b"hi")).unwrap(), b"hi");
    }
}
