//! Nostr identity and signing: the private key (`nsec1...` or 64 hex
//! characters), its x-only public key (hex and `npub1...`), event ids
//! (SHA-256 of the canonical serialization) and BIP-340 Schnorr
//! signatures, for the NIP-42 `AUTH` event (kind 22242) that opens the
//! relay subscription. The sandbox has no random source, so the BIP-340
//! auxiliary data is an HMAC of the message under the key: the nonce stays
//! secret and unique per message, as BIP-340 allows.

use adapter_kit::sign;
use k256::schnorr::SigningKey;
use serde_json::{Value, json};

const CHARSET: &[u8] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const GENERATORS: [u32; 5] = [
    0x3b6a_57b2,
    0x2650_8e6d,
    0x1ea1_19fa,
    0x3d42_33dd,
    0x2a14_62b3,
];

fn polymod(values: &[u8]) -> u32 {
    let mut check = 1u32;
    for value in values {
        let top = check >> 25;
        check = ((check & 0x01ff_ffff) << 5) ^ u32::from(*value);
        for (i, generator) in GENERATORS.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                check ^= generator;
            }
        }
    }
    check
}

fn expand(hrp: &str) -> Vec<u8> {
    let mut out: Vec<u8> = hrp.bytes().map(|b| b >> 5).collect();
    out.push(0);
    out.extend(hrp.bytes().map(|b| b & 31));
    out
}

/// Regroups `data` from `from`-bit to `to`-bit values; `None` on
/// non-zero padding when decoding.
fn regroup(data: &[u8], from: u32, to: u32, pad: bool) -> Option<Vec<u8>> {
    let (mut acc, mut bits, mut out) = (0u32, 0u32, Vec::new());
    let max = (1u32 << to) - 1;
    for value in data {
        acc = (acc << from) | u32::from(*value);
        bits += from;
        while bits >= to {
            bits -= to;
            out.push(u8::try_from((acc >> bits) & max).ok()?);
        }
    }
    if pad && bits > 0 {
        out.push(u8::try_from((acc << (to - bits)) & max).ok()?);
    } else if !pad && (bits >= from || (acc << (to - bits)) & max != 0) {
        return None;
    }
    Some(out)
}

/// The 32 bytes a bech32 string with prefix `hrp` encodes.
fn bech32_decode(text: &str, hrp: &str) -> Option<Vec<u8>> {
    let lower = text.to_lowercase();
    if lower != text && text.to_uppercase() != text {
        return None;
    }
    let (prefix, data) = lower.rsplit_once('1')?;
    if prefix != hrp || data.len() < 6 {
        return None;
    }
    let values: Vec<u8> = data
        .bytes()
        .map(|c| {
            CHARSET
                .iter()
                .position(|x| *x == c)
                .and_then(|p| u8::try_from(p).ok())
        })
        .collect::<Option<_>>()?;
    let mut checked = expand(hrp);
    checked.extend(&values);
    if polymod(&checked) != 1 {
        return None;
    }
    regroup(&values[..values.len() - 6], 5, 8, false).filter(|b| b.len() == 32)
}

/// `bytes` as bech32 with prefix `hrp`.
#[must_use]
pub fn bech32_encode(hrp: &str, bytes: &[u8]) -> String {
    let values = regroup(bytes, 8, 5, true).unwrap_or_default();
    let mut checked = expand(hrp);
    checked.extend(&values);
    checked.extend([0u8; 6]);
    let check = polymod(&checked) ^ 1;
    let mut out = format!("{hrp}1");
    for value in &values {
        out.push(char::from(CHARSET[usize::from(*value)]));
    }
    for i in 0..6 {
        let index = (check >> (5 * (5 - i))) & 31;
        out.push(char::from(CHARSET[index as usize]));
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A signing identity.
pub struct Identity {
    key: SigningKey,
    secret: Vec<u8>,
    /// The x-only public key, hex.
    pub pubkey: String,
}

impl Identity {
    /// The identity of `text` (`nsec1...` or 64 hex characters).
    ///
    /// # Errors
    ///
    /// It is neither, or outside the curve's range.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let secret = if text.to_lowercase().starts_with("nsec1") {
            bech32_decode(text, "nsec").ok_or("the private key is not a valid nsec")?
        } else {
            from_hex(text)
                .filter(|b| b.len() == 32)
                .ok_or("the private key must be nsec1... or 64 hex characters")?
        };
        let key = SigningKey::from_bytes(&secret).map_err(|_| "the private key is out of range")?;
        let pubkey = sign::hex(&key.verifying_key().to_bytes());
        Ok(Self {
            key,
            secret,
            pubkey,
        })
    }

    /// The public key as `npub1...`.
    #[must_use]
    pub fn npub(&self) -> String {
        bech32_encode("npub", &from_hex(&self.pubkey).unwrap_or_default())
    }

    /// A signed event of `kind` with `tags` and `content` at `created_at`.
    ///
    /// # Errors
    ///
    /// The signature could not be made.
    pub fn event(
        &self,
        kind: u64,
        tags: &Value,
        content: &str,
        created_at: u64,
    ) -> Result<Value, String> {
        let canonical = json!([0, self.pubkey, created_at, kind, tags, content]).to_string();
        let id = sign::sha256(canonical.as_bytes());
        let aux: [u8; 32] = sign::hmac_sha256(&self.secret, &id)
            .try_into()
            .map_err(|_| "no auxiliary data")?;
        let signature = self
            .key
            .sign_raw(&id, &aux)
            .map_err(|e| format!("signing failed: {e}"))?;
        Ok(json!({
            "id": sign::hex(&id),
            "pubkey": self.pubkey,
            "created_at": created_at,
            "kind": kind,
            "tags": tags,
            "content": content,
            "sig": sign::hex(&signature.to_bytes()),
        }))
    }

    /// The NIP-42 answer to `challenge` from `relay`.
    ///
    /// # Errors
    ///
    /// As [`Self::event`].
    pub fn auth(&self, relay: &str, challenge: &str, created_at: u64) -> Result<Value, String> {
        let tags = json!([["relay", relay], ["challenge", challenge]]);
        self.event(22242, &tags, "", created_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::schnorr::{Signature, VerifyingKey};

    #[test]
    fn keys_come_as_nsec_or_hex_and_auth_events_verify() {
        // NIP-19's example pair.
        let hex = "67dea2ed018072d675f5415ecfaed7d2597555e202d85b3d65ea4e58d2d92ffa";
        let nsec = "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5";
        let a = Identity::parse(hex).unwrap();
        let b = Identity::parse(nsec).unwrap();
        assert_eq!(a.pubkey, b.pubkey);
        assert_eq!(
            a.pubkey,
            "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e"
        );
        assert_eq!(
            a.npub(),
            "npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg"
        );
        assert!(Identity::parse("nsec1qqqq").is_err());
        let event = a.auth("wss://relay.example", "c-1", 1_700_000_000).unwrap();
        assert_eq!(event["kind"], 22242);
        let id = from_hex(event["id"].as_str().unwrap()).unwrap();
        let canonical =
            json!([0, a.pubkey, 1_700_000_000u64, 22242, event["tags"], ""]).to_string();
        assert_eq!(id, sign::sha256(canonical.as_bytes()));
        let key = VerifyingKey::from_bytes(&from_hex(&a.pubkey).unwrap()).unwrap();
        let signature =
            Signature::try_from(from_hex(event["sig"].as_str().unwrap()).unwrap().as_slice())
                .unwrap();
        key.verify_raw(&id, &signature).unwrap();
    }
}
