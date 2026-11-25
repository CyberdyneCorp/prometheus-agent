//! Encrypted attachments (Matrix spec, Sending encrypted attachments): in
//! an encrypted room a file is uploaded as AES-256-CTR ciphertext and its
//! event carries the key, IV and SHA-256 of the ciphertext under `file`.
//! An inbound one is located as `mxc://…#e2ee=<file JSON, base64url>` so
//! the download can be checked and decrypted.

use base64::Engine as _;
use serde_json::{Value, json};

use super::recovery::ctr;

const FRAGMENT: &str = "#e2ee=";

fn url64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

fn std64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD_NO_PAD
}

fn lenient(text: &str) -> Option<Vec<u8>> {
    let trimmed = text.trim_end_matches('=');
    std64()
        .decode(trimmed)
        .or_else(|_| url64().decode(trimmed))
        .ok()
}

/// `plain` encrypted: the ciphertext to upload and the `file` object
/// (without its `url`).
pub(crate) fn seal(plain: &[u8]) -> Result<(Vec<u8>, Value), String> {
    let random = ring::rand::SystemRandom::new();
    let mut key = [0u8; 32];
    let mut iv = [0u8; 16];
    ring::rand::SecureRandom::fill(&random, &mut key).map_err(|_| "no randomness")?;
    // The counter half of the IV starts at zero.
    ring::rand::SecureRandom::fill(&random, &mut iv[..8]).map_err(|_| "no randomness")?;
    let ciphertext = ctr(&key, &iv, plain);
    let hash = ring::digest::digest(&ring::digest::SHA256, &ciphertext);
    let file = json!({
        "key": {"kty": "oct", "key_ops": ["encrypt", "decrypt"], "alg": "A256CTR",
                "k": url64().encode(key), "ext": true},
        "iv": std64().encode(iv),
        "hashes": {"sha256": std64().encode(hash.as_ref())},
        "v": "v2",
    });
    Ok((ciphertext, file))
}

/// Where an encrypted attachment is fetched from (its `url` with the
/// decryption details).
pub(crate) fn location(file: &Value) -> Option<String> {
    let url = file["url"].as_str()?;
    Some(format!(
        "{url}{FRAGMENT}{}",
        url64().encode(file.to_string())
    ))
}

/// The `mxc://` URI of a location, and its decryption details when it is
/// an encrypted attachment.
pub(crate) fn split(location: &str) -> (&str, Option<Value>) {
    match location.split_once(FRAGMENT) {
        Some((url, details)) => {
            let file = url64()
                .decode(details)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            (url, file)
        }
        None => (location, None),
    }
}

/// The plaintext of a downloaded encrypted attachment, its hash checked.
pub(crate) fn open(ciphertext: &[u8], file: &Value) -> Result<Vec<u8>, String> {
    let key: [u8; 32] = file["key"]["k"]
        .as_str()
        .and_then(lenient)
        .and_then(|k| k.try_into().ok())
        .ok_or("the attachment key is not 32 bytes")?;
    let iv: [u8; 16] = file["iv"]
        .as_str()
        .and_then(lenient)
        .and_then(|k| k.try_into().ok())
        .ok_or("the attachment IV is not 16 bytes")?;
    let expected = file["hashes"]["sha256"]
        .as_str()
        .and_then(lenient)
        .ok_or("the attachment has no SHA-256")?;
    let actual = ring::digest::digest(&ring::digest::SHA256, ciphertext);
    if actual.as_ref() != expected.as_slice() {
        return Err("the attachment does not match its SHA-256".into());
    }
    Ok(ctr(&key, &iv, ciphertext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_file_opens_and_a_tampered_one_does_not() {
        let (mut ciphertext, mut file) = seal(b"secret report").unwrap();
        assert_ne!(ciphertext, b"secret report");
        file["url"] = json!("mxc://hs/abc");
        let located = location(&file).unwrap();
        let (url, details) = split(&located);
        assert_eq!(url, "mxc://hs/abc");
        let details = details.unwrap();
        assert_eq!(open(&ciphertext, &details).unwrap(), b"secret report");
        ciphertext[0] ^= 1;
        assert!(open(&ciphertext, &details).is_err());
    }
}
