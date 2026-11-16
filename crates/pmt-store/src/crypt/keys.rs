//! Where a store's key comes from (spec: session-storage, Encryption at
//! rest option): the OS keychain (`storage.encryption = "keychain"`, a
//! random key stored as `STATE_DB_KEY` of the profile) or a passphrase
//! (`"passphrase"`: the `STATE_DB_PASSPHRASE` secret, stretched with
//! Argon2id and the per-profile salt in `state.db.salt`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use argon2::{Algorithm, Argon2, Params, Version};
use pmt_config::ProfileScope;
use pmt_config::schema::{SecretsBackend, StorageEncryption};
use pmt_config::secrets::{Keychain, SecretStore};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;

use super::{Crypt, KEY_UNAVAILABLE, Key};

/// The keychain secret holding a profile's store key (64 hex digits).
pub const KEY_SECRET: &str = "STATE_DB_KEY";
/// The secret a passphrase-encrypted store derives its key from.
pub const PASSPHRASE_SECRET: &str = "STATE_DB_PASSPHRASE";

/// The salt file beside `database`.
#[must_use]
pub fn salt_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".salt");
    PathBuf::from(name)
}

fn unavailable(why: &str) -> Error {
    Error::new(&codes::STORE_KEY_UNAVAILABLE, KEY_UNAVAILABLE).with_hint(why.to_owned())
}

fn hex(key: &Key) -> String {
    use std::fmt::Write as _;
    key.bytes()
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn parse_hex(text: &str) -> Option<Key> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(Key::new(bytes))
}

/// The key of `scope`'s store at `database` under `mode`; `create` makes a
/// missing one (a new store, or `sessions encrypt`). `None` for `none`.
///
/// # Errors
///
/// `store.key_unavailable` (exit 78) when the key cannot be had.
pub fn resolve(
    scope: &ProfileScope,
    keychain: &Arc<dyn Keychain>,
    mode: StorageEncryption,
    database: &Path,
    create: bool,
) -> Result<Option<Crypt>, Error> {
    let key = match mode {
        StorageEncryption::None => return Ok(None),
        StorageEncryption::Keychain => keychain_key(scope, keychain, create)?,
        StorageEncryption::Passphrase => passphrase_key(scope, database, create)?,
    };
    Crypt::new(key).map(Some)
}

fn keychain_key(
    scope: &ProfileScope,
    keychain: &Arc<dyn Keychain>,
    create: bool,
) -> Result<Key, Error> {
    let store = SecretStore::with_keychain(
        scope.profile(),
        SecretsBackend::Keychain,
        Arc::clone(keychain),
    );
    let stored = store.get(KEY_SECRET).map_err(|e| {
        unavailable(&format!(
            "the OS keychain could not be read: {}",
            e.message()
        ))
    })?;
    if let Some(value) = stored {
        return parse_hex(value.expose())
            .ok_or_else(|| unavailable("the keychain entry STATE_DB_KEY is not a 256-bit key"));
    }
    if !create {
        return Err(unavailable(
            "the keychain has no STATE_DB_KEY for this profile; restore it, or restore an unencrypted backup",
        ));
    }
    let key = Key::generate().map_err(|e| unavailable(&e))?;
    store
        .set(KEY_SECRET, &SecretString::new(hex(&key)))
        .map_err(|e| unavailable(&format!("the OS keychain refused the key: {}", e.message())))?;
    Ok(key)
}

fn passphrase_key(scope: &ProfileScope, database: &Path, create: bool) -> Result<Key, Error> {
    let passphrase = scope.secrets().resolve(PASSPHRASE_SECRET).map_err(|_| {
        unavailable(
            "set the STATE_DB_PASSPHRASE secret (prometheus secrets set STATE_DB_PASSPHRASE)",
        )
    })?;
    let salt_file = salt_path(database);
    let salt = match std::fs::read(&salt_file) {
        Ok(salt) if salt.len() == 16 => salt,
        Ok(_) => return Err(unavailable("state.db.salt is damaged")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
            let mut salt = vec![0u8; 16];
            getrandom::fill(&mut salt).map_err(|e| unavailable(&e.to_string()))?;
            crate::files::create_private(&salt_file)?;
            std::fs::write(&salt_file, &salt)
                .map_err(|e| Error::io(&codes::STORE_IO, &salt_file, &e))?;
            salt
        }
        Err(_) => return Err(unavailable("state.db.salt is missing")),
    };
    derive(passphrase.expose(), &salt)
}

/// The key a passphrase and salt give (Argon2id, 19 MiB, 2 passes).
///
/// # Errors
///
/// Argon2 refused the parameters.
pub fn derive(passphrase: &str, salt: &[u8]) -> Result<Key, Error> {
    let params = Params::new(19 * 1024, 2, 1, Some(32)).map_err(|e| unavailable(&e.to_string()))?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|e| unavailable(&e.to_string()))?;
    Ok(Key::new(out))
}

/// The marker beside an encrypted `database` (its mode's name): an
/// unreadable file without one is corruption, with one it is a store
/// whose key is missing.
#[must_use]
pub fn marker_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".encryption");
    PathBuf::from(name)
}

/// Records (or, for `None`, removes) the marker of `database`.
///
/// # Errors
///
/// `store.io`.
pub fn mark(database: &Path, mode: StorageEncryption) -> Result<(), Error> {
    let marker = marker_path(database);
    let name = match mode {
        StorageEncryption::None => {
            return match std::fs::remove_file(&marker) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    Err(Error::io(&codes::STORE_IO, &marker, &e))
                }
                _ => Ok(()),
            };
        }
        StorageEncryption::Keychain => "keychain",
        StorageEncryption::Passphrase => "passphrase",
    };
    pmt_core::private_fs::write_atomic(&marker, name.as_bytes())
        .map_err(|e| Error::io(&codes::STORE_IO, &marker, &e))
}

/// The encryption of `scope`'s store at `database`, checked against the
/// file: a new store gets a new key; an existing encrypted one needs its
/// key, and that key must open its first page (a wrong key is never taken
/// for corruption); a plaintext file under an encryption mode (or an
/// encrypted one under `none`) is refused, so nothing is ever silently
/// created, quarantined or converted.
///
/// # Errors
///
/// `store.key_unavailable`, `store.encryption_mismatch` (exit 78).
pub fn for_store(
    scope: &ProfileScope,
    keychain: &Arc<dyn Keychain>,
    database: &Path,
) -> Result<Option<Crypt>, Error> {
    let mode = scope.config().storage.encryption;
    let marked = marker_path(database).is_file();
    match (mode, super::is_plaintext(database)) {
        (StorageEncryption::None, Some(false)) if marked => Err(unavailable(
            "storage.encryption is none but state.db is encrypted; set storage.encryption to the store's mode",
        )),
        (StorageEncryption::None, _) => Ok(None),
        (_, Some(true)) => Err(Error::new(
            &codes::STORE_ENCRYPTION_MISMATCH,
            "state.db is not encrypted but storage.encryption asks for it",
        )
        .with_hint("convert it offline: prometheus sessions encrypt")),
        (mode, None) => {
            let crypt = resolve(scope, keychain, mode, database, true)?;
            mark(database, mode)?;
            Ok(crypt)
        }
        (mode, Some(false)) => {
            let crypt = resolve(scope, keychain, mode, database, false)?;
            if let Some(crypt) = &crypt
                && !super::opens_first_page(database, crypt.key())
            {
                return Err(unavailable(
                    "the key does not open state.db (another keychain entry or passphrase)",
                ));
            }
            Ok(crypt)
        }
    }
}

/// [`for_store`] on the OS keychain for an existing store (offline
/// maintenance never creates a key).
///
/// # Errors
///
/// As [`for_store`].
pub fn existing(scope: &ProfileScope, database: &Path) -> Result<Option<Crypt>, Error> {
    let keychain: Arc<dyn Keychain> = Arc::new(pmt_config::secrets::OsKeychain);
    for_store(scope, &keychain, database)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_keys_round_trip() {
        let key = Key::generate().unwrap();
        assert_eq!(parse_hex(&hex(&key)).unwrap().bytes(), key.bytes());
        assert!(parse_hex("zz").is_none());
    }

    #[test]
    fn a_passphrase_derives_the_same_key_for_the_same_salt_only() {
        let a = derive("correct horse", &[1; 16]).unwrap();
        let b = derive("correct horse", &[1; 16]).unwrap();
        let c = derive("correct horse", &[2; 16]).unwrap();
        assert_eq!(a.bytes(), b.bytes());
        assert_ne!(a.bytes(), c.bytes());
    }
}
