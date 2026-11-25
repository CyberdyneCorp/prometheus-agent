//! The ciphers the WeCom and Weixin mocks seal payloads with, as the
//! platforms do: `WXBizMsgCrypt` (AES-256-CBC, `PKCS#7` to 32, SHA1
//! signature) and AES-128-ECB.

#![allow(dead_code)]

use aes::cipher::{BlockDecrypt as _, BlockEncrypt as _, KeyInit as _};
use base64::Engine as _;
use cbc::cipher::{BlockEncryptMut as _, KeyIvInit as _, block_padding::NoPadding};

fn pad(mut data: Vec<u8>, block: usize) -> Vec<u8> {
    let n = block - data.len() % block;
    data.extend(std::iter::repeat_n(u8::try_from(n).unwrap(), n));
    data
}

/// An EncodingAESKey (43 characters) and the 32-byte key it stands for.
pub fn encoding_aes_key(seed: u8) -> (String, [u8; 32]) {
    let key = [seed; 32];
    let text = base64::engine::general_purpose::STANDARD.encode(key);
    (text.trim_end_matches('=').to_owned(), key)
}

/// The SHA1 signature of a callback.
pub fn signature(token: &str, timestamp: &str, nonce: &str, encrypt: &str) -> String {
    let mut parts = [token, timestamp, nonce, encrypt];
    parts.sort_unstable();
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        parts.concat().as_bytes(),
    );
    digest.as_ref().iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

/// `xml` sealed for `receiver` with `key` (the `Encrypt` field).
pub fn seal(key: &[u8; 32], xml: &str, receiver: &str) -> String {
    let mut plain = b"0123456789abcdef".to_vec();
    plain.extend(u32::try_from(xml.len()).unwrap().to_be_bytes());
    plain.extend(xml.as_bytes());
    plain.extend(receiver.as_bytes());
    let mut buffer = pad(plain, 32);
    let len = buffer.len();
    cbc::Encryptor::<aes::Aes256>::new_from_slices(key, &key[..16])
        .unwrap()
        .encrypt_padded_mut::<NoPadding>(&mut buffer, len)
        .unwrap();
    base64::engine::general_purpose::STANDARD.encode(buffer)
}

/// AES-128-ECB with `PKCS#7`.
pub fn ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = aes::Aes128::new_from_slice(key).unwrap();
    let mut out = pad(data.to_vec(), 16);
    for block in out.chunks_mut(16) {
        cipher.encrypt_block(block.into());
    }
    out
}

/// The plaintext of [`ecb_encrypt`].
pub fn ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = aes::Aes128::new_from_slice(key).unwrap();
    let mut out = data.to_vec();
    for block in out.chunks_mut(16) {
        cipher.decrypt_block(block.into());
    }
    let pad = usize::from(*out.last().unwrap());
    out.truncate(out.len() - pad);
    out
}

/// AES-256-CBC with `PKCS#7` to 32 (WeCom media; IV = the key's first 16).
pub fn cbc_encrypt(key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let mut buffer = pad(data.to_vec(), 32);
    let len = buffer.len();
    cbc::Encryptor::<aes::Aes256>::new_from_slices(key, &key[..16])
        .unwrap()
        .encrypt_padded_mut::<NoPadding>(&mut buffer, len)
        .unwrap();
    buffer
}
