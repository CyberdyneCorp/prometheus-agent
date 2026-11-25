//! The symmetric ciphers platforms wrap payloads in: AES-CBC (`WeCom`
//! callbacks and media, `WXBizMsgCrypt`, with PKCS#7 padding to a block
//! of 32) and AES-128-ECB (Weixin CDN media, PKCS#7 to 16).

use aes::cipher::{BlockDecrypt as _, BlockEncrypt as _, KeyInit as _, KeyIvInit as _};
use cbc::cipher::{BlockDecryptMut as _, BlockEncryptMut as _, block_padding::NoPadding};

/// Pads `data` to a multiple of `block` with PKCS#7.
#[must_use]
pub fn pad(mut data: Vec<u8>, block: usize) -> Vec<u8> {
    let n = block - data.len() % block;
    #[allow(clippy::cast_possible_truncation)]
    data.extend(std::iter::repeat_n(n as u8, n));
    data
}

/// Removes PKCS#7 padding of at most `block` bytes.
///
/// # Errors
///
/// The padding is malformed.
pub fn unpad(mut data: Vec<u8>, block: usize) -> Result<Vec<u8>, String> {
    let n = usize::from(*data.last().ok_or("empty plaintext")?);
    if n == 0 || n > block || n > data.len() {
        return Err("bad padding".to_owned());
    }
    data.truncate(data.len() - n);
    Ok(data)
}

/// AES-256-CBC decryption (no padding removed).
///
/// # Errors
///
/// A key that is not 32 bytes, an IV that is not 16, or data that is not
/// whole blocks.
pub fn aes256_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = cbc::Decryptor::<aes::Aes256>::new_from_slices(key, iv)
        .map_err(|_| "AES-256-CBC needs a 32-byte key and a 16-byte IV".to_owned())?;
    let mut buffer = data.to_vec();
    cipher
        .decrypt_padded_mut::<NoPadding>(&mut buffer)
        .map(<[u8]>::to_vec)
        .map_err(|_| "ciphertext is not whole AES blocks".to_owned())
}

/// AES-256-CBC encryption of whole blocks (pad first).
///
/// # Errors
///
/// As [`aes256_cbc_decrypt`].
pub fn aes256_cbc_encrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = cbc::Encryptor::<aes::Aes256>::new_from_slices(key, iv)
        .map_err(|_| "AES-256-CBC needs a 32-byte key and a 16-byte IV".to_owned())?;
    let mut buffer = data.to_vec();
    let len = buffer.len();
    cipher
        .encrypt_padded_mut::<NoPadding>(&mut buffer, len)
        .map(<[u8]>::to_vec)
        .map_err(|_| "plaintext is not whole AES blocks".to_owned())
}

/// AES-128-ECB decryption with PKCS#7.
///
/// # Errors
///
/// A key that is not 16 bytes, data that is not whole blocks or bad
/// padding.
pub fn aes128_ecb_decrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = aes::Aes128::new_from_slice(key).map_err(|_| "AES-128 needs a 16-byte key")?;
    if !data.len().is_multiple_of(16) {
        return Err("ciphertext is not whole AES blocks".to_owned());
    }
    let mut out = data.to_vec();
    for block in out.chunks_mut(16) {
        cipher.decrypt_block(block.into());
    }
    unpad(out, 16)
}

/// AES-128-ECB encryption with PKCS#7.
///
/// # Errors
///
/// A key that is not 16 bytes.
pub fn aes128_ecb_encrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = aes::Aes128::new_from_slice(key).map_err(|_| "AES-128 needs a 16-byte key")?;
    let mut out = pad(data.to_vec(), 16);
    for block in out.chunks_mut(16) {
        cipher.encrypt_block(block.into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ciphers_round_trip_and_match_a_known_vector() {
        // FIPS-197 C.1: AES-128 of 00112233..ff under 00010203..0f.
        let key: Vec<u8> = (0u8..16).collect();
        let block: Vec<u8> = (0u8..16).map(|i| i * 0x11).collect();
        let sealed = aes128_ecb_encrypt(&key, &block).unwrap();
        assert_eq!(
            crate::sign::hex(&sealed[..16]),
            "69c4e0d86a7b0430d8cdb78070b4c55a"
        );
        assert_eq!(aes128_ecb_decrypt(&key, &sealed).unwrap(), block);
        let key32 = [7u8; 32];
        let iv = [1u8; 16];
        let plain = pad(b"hello wecom".to_vec(), 32);
        let sealed = aes256_cbc_encrypt(&key32, &iv, &plain).unwrap();
        let opened = aes256_cbc_decrypt(&key32, &iv, &sealed).unwrap();
        assert_eq!(unpad(opened, 32).unwrap(), b"hello wecom");
        assert!(unpad(vec![1, 2, 40], 32).is_err());
    }
}
