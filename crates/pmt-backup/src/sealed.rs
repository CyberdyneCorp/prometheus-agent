//! Passphrase sealing of exported secrets (spec: configuration-and-
//! profiles, Profile export excludes credentials by default): the key is
//! derived from the passphrase with Argon2id over a random salt, and the
//! secrets are encrypted with XChaCha20-Poly1305 under a random 24-byte
//! nonce. The sealed document records its parameters, so later builds can
//! raise the cost without breaking older archives.

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::Aead as _;
use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305, XNonce};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde::{Deserialize, Serialize};

/// Argon2id memory cost in KiB (OWASP's 19 MiB, 2 passes baseline).
const M_COST: u32 = 19_456;
/// Argon2id passes.
const T_COST: u32 = 2;
/// Argon2id lanes.
const P_COST: u32 = 1;

/// The sealed document, stored as JSON in the archive.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sealed {
    kdf: String,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    salt: String,
    cipher: String,
    nonce: String,
    ciphertext: String,
}

/// Encrypts `plaintext` under `passphrase`.
///
/// # Errors
///
/// `profile.archive_invalid` when the OS has no randomness or the key
/// cannot be derived.
pub fn seal(passphrase: &SecretString, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
    let mut salt = [0_u8; 16];
    let mut nonce = [0_u8; 24];
    getrandom::fill(&mut salt).map_err(|e| failed(&e))?;
    getrandom::fill(&mut nonce).map_err(|e| failed(&e))?;
    let key = derive(passphrase, &salt, M_COST, T_COST, P_COST)?;
    let cipher = XChaCha20Poly1305::new(&key.into());
    let ciphertext = cipher
        .encrypt(&XNonce::from(nonce), plaintext)
        .map_err(|e| failed(&e))?;
    let sealed = Sealed {
        kdf: "argon2id".into(),
        m_cost: M_COST,
        t_cost: T_COST,
        p_cost: P_COST,
        salt: STANDARD.encode(salt),
        cipher: "xchacha20poly1305".into(),
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
    };
    serde_json::to_vec_pretty(&sealed).map_err(|e| failed(&e))
}

/// Decrypts what [`seal`] produced.
///
/// # Errors
///
/// `profile.archive_passphrase` (78) for another passphrase (or tampered
/// data); `profile.archive_invalid` for a malformed document.
pub fn open(passphrase: &SecretString, sealed: &[u8]) -> Result<Vec<u8>, Error> {
    let sealed: Sealed = serde_json::from_slice(sealed).map_err(|e| invalid(&e))?;
    if sealed.kdf != "argon2id" || sealed.cipher != "xchacha20poly1305" {
        return Err(invalid(&"unknown key derivation or cipher"));
    }
    let salt = STANDARD.decode(&sealed.salt).map_err(|e| invalid(&e))?;
    let nonce: [u8; 24] = STANDARD
        .decode(&sealed.nonce)
        .map_err(|e| invalid(&e))?
        .try_into()
        .map_err(|_| invalid(&"the nonce is not 24 bytes"))?;
    let ciphertext = STANDARD
        .decode(&sealed.ciphertext)
        .map_err(|e| invalid(&e))?;
    let key = derive(
        passphrase,
        &salt,
        sealed.m_cost,
        sealed.t_cost,
        sealed.p_cost,
    )?;
    XChaCha20Poly1305::new(&key.into())
        .decrypt(&XNonce::from(nonce), ciphertext.as_slice())
        .map_err(|_| {
            Error::new(
                &codes::PROFILE_ARCHIVE_PASSPHRASE,
                "the passphrase does not open the archive's secrets",
            )
        })
}

fn derive(
    passphrase: &SecretString,
    salt: &[u8],
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<[u8; 32], Error> {
    let params = Params::new(m_cost, t_cost, p_cost, Some(32)).map_err(|e| invalid(&e))?;
    let mut key = [0_u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.expose().as_bytes(), salt, &mut key)
        .map_err(|e| invalid(&e))?;
    Ok(key)
}

fn failed(e: &dyn std::fmt::Display) -> Error {
    Error::new(
        &codes::PROFILE_ARCHIVE_INVALID,
        format!("cannot seal the secrets: {e}"),
    )
}

fn invalid(e: &dyn std::fmt::Display) -> Error {
    Error::new(
        &codes::PROFILE_ARCHIVE_INVALID,
        format!("the archive's sealed secrets are malformed: {e}"),
    )
}

#[cfg(test)]
mod tests {
    use pmt_core::secret::SecretString;

    #[test]
    fn only_the_same_passphrase_opens_what_was_sealed() {
        let pass = SecretString::new("correct horse battery staple");
        let sealed = super::seal(&pass, b"TOKEN = \"value\"").unwrap();
        assert!(!String::from_utf8_lossy(&sealed).contains("value"));
        assert_eq!(super::open(&pass, &sealed).unwrap(), b"TOKEN = \"value\"");
        let wrong = SecretString::new("wrong");
        let err = super::open(&wrong, &sealed).unwrap_err();
        assert_eq!(err.code().code, "profile.archive_passphrase");
    }
}
