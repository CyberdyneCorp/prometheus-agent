//! TOTP codes (RFC 6238 over RFC 4226): HMAC-SHA1 of the 30-second time
//! step, 6 digits, from a base32 seed as sites show it.

use sha1::{Digest as _, Sha1};

/// Seconds per step.
const STEP: u64 = 30;

/// The code of `seed` at Unix time `now`; `None` for a seed that is not
/// base32.
#[must_use]
pub fn code(seed: &str, now: u64) -> Option<String> {
    let normalized: String = seed
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let secret = data_encoding::BASE32_NOPAD
        .decode(normalized.trim_end_matches('=').as_bytes())
        .ok()?;
    if secret.is_empty() {
        return None;
    }
    let mac = hmac_sha1(&secret, &(now / STEP).to_be_bytes());
    let offset = usize::from(mac[19] & 0x0f);
    let binary = u32::from_be_bytes([
        mac[offset] & 0x7f,
        mac[offset + 1],
        mac[offset + 2],
        mac[offset + 3],
    ]);
    Some(format!("{:06}", binary % 1_000_000))
}

fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..20].copy_from_slice(&Sha1::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha1::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha1::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

#[cfg(test)]
mod tests {
    use super::code;

    /// RFC 6238 appendix B (SHA-1 seed "12345678901234567890"), last six
    /// digits of the eight-digit vectors.
    #[test]
    fn rfc_6238_vectors() {
        let seed = data_encoding::BASE32_NOPAD.encode(b"12345678901234567890");
        for (time, want) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
        ] {
            assert_eq!(code(&seed, time).as_deref(), Some(want), "{time}");
        }
        assert_eq!(code("not base32!", 59), None);
    }
}
