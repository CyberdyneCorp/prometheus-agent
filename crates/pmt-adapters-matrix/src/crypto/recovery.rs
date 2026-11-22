//! Cross-signing from secret storage (spec: platform-adapters, Matrix
//! end-to-end encryption): with the secret `MATRIX_RECOVERY_KEY` the
//! adapter reads the account's self-signing key from secret storage
//! (`m.secret_storage.v1.aes-hmac-sha2`: HKDF-SHA-256, AES-256-CTR,
//! HMAC-SHA-256), checks it against the published one and signs this
//! device with it on every start, so other users' clients trust it.

use aes::cipher::{BlockEncrypt as _, KeyInit as _};
use base64::Engine as _;
use serde_json::{Value, json};
use vodozemac::Ed25519SecretKey;

use super::signed;
use crate::api::{Api, segment};

/// The secret holding the account's recovery key.
pub const RECOVERY_SECRET: &str = "MATRIX_RECOVERY_KEY";
const ALGORITHM: &str = "m.secret_storage.v1.aes-hmac-sha2";
const SELF_SIGNING: &str = "m.cross_signing.self_signing";
const BITCOIN: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(text: &str) -> Result<Vec<u8>, String> {
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    engine.decode(text).map_err(|e| format!("bad base64: {e}"))
}

fn base58_decode(text: &str) -> Option<Vec<u8>> {
    let mut number: Vec<u8> = Vec::new(); // little-endian base 256
    for c in text.bytes() {
        let mut carry = u32::try_from(BITCOIN.iter().position(|&a| a == c)?).ok()?;
        for byte in &mut number {
            carry += u32::from(*byte) * 58;
            *byte = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            number.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let zeros = text.bytes().take_while(|&c| c == b'1').count();
    let mut out = vec![0; zeros];
    out.extend(number.iter().rev());
    Some(out)
}

/// A recovery key as Element shows it (base58, spaces allowed); the
/// mock's (tests make accounts with secret storage).
#[cfg(any(test, feature = "kit"))]
pub(crate) fn encode_recovery_key(key: &[u8; 32]) -> String {
    let mut bytes = vec![0x8b, 0x01];
    bytes.extend_from_slice(key);
    bytes.push(bytes.iter().fold(0, |p, b| p ^ b));
    let mut digits: Vec<u8> = Vec::new(); // little-endian base 58
    for &byte in &bytes {
        let mut carry = u32::from(byte);
        for digit in &mut digits {
            carry += u32::from(*digit) << 8;
            *digit = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let zeros = bytes.iter().take_while(|&&b| b == 0).count();
    let text: String = std::iter::repeat_n('1', zeros)
        .chain(
            digits
                .iter()
                .rev()
                .map(|&d| char::from(BITCOIN[usize::from(d)])),
        )
        .collect();
    let groups: Vec<String> = text
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    groups.join(" ")
}

/// The 32-byte key of a recovery key.
pub(crate) fn decode_recovery_key(text: &str) -> Result<[u8; 32], String> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base58_decode(&compact).ok_or("the recovery key is not base58")?;
    let parity = bytes.iter().fold(0, |p, b| p ^ b);
    if bytes.len() != 35 || bytes[..2] != [0x8b, 0x01] || parity != 0 {
        return Err("the recovery key is not a Matrix recovery key (bad prefix or parity)".into());
    }
    let mut key = [0; 32];
    key.copy_from_slice(&bytes[2..34]);
    Ok(key)
}

/// The AES and HMAC keys of secret `name`.
fn derive(key: &[u8; 32], name: &str) -> ([u8; 32], [u8; 32]) {
    struct Len64;
    impl ring::hkdf::KeyType for Len64 {
        fn len(&self) -> usize {
            64
        }
    }
    let info = [name.as_bytes()];
    let mut out = [0; 64];
    ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, &[0; 32])
        .extract(key)
        .expand(&info, Len64)
        .and_then(|okm| okm.fill(&mut out))
        .expect("64 bytes is a valid HKDF-SHA-256 length");
    let (mut aes, mut mac) = ([0; 32], [0; 32]);
    aes.copy_from_slice(&out[..32]);
    mac.copy_from_slice(&out[32..]);
    (aes, mac)
}

/// AES-256-CTR with the 128-bit big-endian counter `iv` (decrypting and
/// encrypting are the same).
pub(super) fn ctr(key: &[u8; 32], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = aes::Aes256::new(key.into());
    let mut counter = u128::from_be_bytes(*iv);
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block = counter.to_be_bytes().into();
        cipher.encrypt_block(&mut block);
        out.extend(chunk.iter().zip(block.iter()).map(|(d, k)| d ^ k));
        counter = counter.wrapping_add(1);
    }
    out
}

fn hmac(key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), data)
        .as_ref()
        .to_vec()
}

/// Encrypts secret `name` (`{iv, ciphertext, mac}`, as secret storage
/// holds it) with `iv`.
pub(crate) fn encrypt_secret(key: &[u8; 32], name: &str, plaintext: &[u8], iv: [u8; 16]) -> Value {
    let mut iv = iv;
    iv[8] &= 0x7f; // bit 63 clear, as the spec asks
    let (aes, mac_key) = derive(key, name);
    let ciphertext = ctr(&aes, &iv, plaintext);
    json!({"iv": b64(&iv), "ciphertext": b64(&ciphertext), "mac": b64(&hmac(&mac_key, &ciphertext))})
}

/// Decrypts secret `name` from its `{iv, ciphertext, mac}`.
pub(crate) fn decrypt_secret(
    key: &[u8; 32],
    name: &str,
    encrypted: &Value,
) -> Result<Vec<u8>, String> {
    let field = |f: &str| {
        encrypted[f]
            .as_str()
            .ok_or_else(|| format!("{name} has no {f}"))
            .and_then(unb64)
    };
    let iv: [u8; 16] = field("iv")?
        .try_into()
        .map_err(|_| format!("{name}: the iv is not 16 bytes"))?;
    let (ciphertext, mac) = (field("ciphertext")?, field("mac")?);
    let (aes, mac_key) = derive(key, name);
    let ok = ring::hmac::verify(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &mac_key),
        &ciphertext,
        &mac,
    );
    if ok.is_err() {
        return Err(format!(
            "{name}: the recovery key does not open it (MAC mismatch)"
        ));
    }
    Ok(ctr(&aes, &iv, &ciphertext))
}

/// The description of a secret storage key whose `iv` and `mac` prove a
/// recovery key opens it (the mock's).
#[cfg(any(test, feature = "kit"))]
pub(crate) fn key_description(key: &[u8; 32], iv: [u8; 16]) -> Value {
    let check = encrypt_secret(key, "", &[0; 32], iv);
    json!({"algorithm": ALGORITHM, "iv": check["iv"], "mac": check["mac"]})
}

async fn account_data(api: &Api, user: &str, kind: &str) -> Result<Value, String> {
    let path = format!("/user/{}/account_data/{}", segment(user), segment(kind));
    api.get(&path)
        .await
        .map_err(|e| format!("account data {kind}: {}", e.describe()))
}

/// The self-signing key secret storage holds, opened with `key`.
async fn self_signing_key(
    api: &Api,
    user: &str,
    key: &[u8; 32],
) -> Result<Ed25519SecretKey, String> {
    let default = account_data(api, user, "m.secret_storage.default_key").await?;
    let id = default["key"]
        .as_str()
        .ok_or("no default secret storage key")?;
    let description = account_data(api, user, &format!("m.secret_storage.key.{id}")).await?;
    if description["algorithm"] != ALGORITHM {
        return Err(format!("secret storage key {id} is not {ALGORITHM}"));
    }
    if description["mac"].is_string() {
        let check = encrypt_secret(
            key,
            "",
            &[0; 32],
            unb64(description["iv"].as_str().unwrap_or_default())?
                .try_into()
                .map_err(|_| "the key description's iv is not 16 bytes")?,
        );
        if check["mac"].as_str().map(unb64) != description["mac"].as_str().map(unb64) {
            return Err("the recovery key does not match the account's secret storage".into());
        }
    }
    let secret = account_data(api, user, SELF_SIGNING).await?;
    let seed = decrypt_secret(key, SELF_SIGNING, &secret["encrypted"][id])?;
    let seed = unb64(String::from_utf8_lossy(&seed).trim())?;
    let seed: [u8; 32] = seed
        .try_into()
        .map_err(|_| "the self-signing key is not 32 bytes")?;
    Ok(Ed25519SecretKey::from_slice(&seed))
}

/// Signs `device_keys` (this device's signed keys) with the self-signing
/// key from secret storage, after checking it is the published one.
///
/// # Errors
///
/// Why the device was not signed.
pub(crate) async fn self_sign(
    api: &Api,
    user: &str,
    device: &str,
    device_keys: &Value,
    recovery_key: &str,
) -> Result<(), String> {
    let key = decode_recovery_key(recovery_key)?;
    let ssk = self_signing_key(api, user, &key).await?;
    let public = ssk.public_key().to_base64();
    let query = api
        .post("/keys/query", &json!({"device_keys": {user: []}}))
        .await
        .map_err(|e| format!("keys/query: {}", e.describe()))?;
    let key_id = format!("ed25519:{public}");
    if query["self_signing_keys"][user]["keys"][&key_id].as_str() != Some(public.as_str()) {
        return Err("the self-signing key in secret storage is not the published one".into());
    }
    let mut signed_keys = device_keys.clone();
    let signature = ssk
        .sign(signed::signable(device_keys).as_bytes())
        .to_base64();
    signed::add_signature(&mut signed_keys, user, &key_id, &signature);
    api.post(
        "/keys/signatures/upload",
        &json!({user: {device: signed_keys}}),
    )
    .await
    .map(drop)
    .map_err(|e| format!("keys/signatures/upload: {}", e.describe()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recovery_key_round_trips_and_a_typo_is_refused() {
        let key = [7u8; 32];
        let text = encode_recovery_key(&key);
        assert!(text.starts_with("Es"), "{text}");
        assert_eq!(decode_recovery_key(&text).unwrap(), key);
        let mut typo = text.clone();
        typo.replace_range(5..6, if &text[5..6] == "a" { "b" } else { "a" });
        assert!(decode_recovery_key(&typo).is_err());
    }

    #[test]
    fn a_secret_opens_only_with_its_key_and_name() {
        let key = [3u8; 32];
        let sealed = encrypt_secret(&key, SELF_SIGNING, b"seed", [9; 16]);
        assert_eq!(
            decrypt_secret(&key, SELF_SIGNING, &sealed).unwrap(),
            b"seed"
        );
        assert!(decrypt_secret(&[4; 32], SELF_SIGNING, &sealed).is_err());
        assert!(decrypt_secret(&key, "m.cross_signing.master", &sealed).is_err());
    }
}
