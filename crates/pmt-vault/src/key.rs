//! The vault key (spec: security-boundaries, Credential vault storage):
//! 32 random bytes, kept either in `<vault>/vault.key` (hex, 0600, never
//! followed through a link) or in an OS keychain entry whose account name
//! `<vault>/vault.keyref` records.
//!
//! The keyref decides: present, the keychain holds the key; absent, the
//! file does. Moving the key therefore always writes the target first,
//! checks that it decrypts the vault, flips the keyref and only then
//! removes the old copy, so an interruption at any step leaves a vault
//! that opens with the key it had, and running the move again finishes it.

use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use pmt_config::secrets::Keychain;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use sha2::{Digest as _, Sha256};

/// The key file name.
pub const KEY_FILE: &str = "vault.key";
/// The keychain reference file name.
pub const KEYREF_FILE: &str = "vault.keyref";
/// What a vault whose key is missing fails with.
pub const KEY_NOT_FOUND: &str = "vault key not found; restore it or delete vault.enc to start over";

/// A vault key.
#[derive(Clone, PartialEq, Eq)]
pub struct Key(pub(crate) [u8; 32]);

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

impl Key {
    /// A new random key.
    ///
    /// # Panics
    ///
    /// When the OS random generator is unavailable.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("the OS random generator is available");
        Self(bytes)
    }

    fn hex(&self) -> String {
        use std::fmt::Write as _;
        self.0.iter().fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
    }

    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.len() != 64 {
            return None;
        }
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
        }
        Some(Self(key))
    }
}

/// Where the key lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// `vault.key`.
    File(PathBuf),
    /// The keychain account named in `vault.keyref`.
    Keychain(String),
}

impl Location {
    /// `file` or `keychain`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::File(_) => "file",
            Self::Keychain(_) => "keychain",
        }
    }
}

/// The keychain account of the vault in `dir` (unique per vault path).
#[must_use]
pub fn account(dir: &Path) -> String {
    let digest = Sha256::digest(dir.to_string_lossy().as_bytes());
    let hex: String = digest.iter().take(8).fold(String::new(), |mut out, b| {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
        out
    });
    format!("vault-{hex}")
}

fn io_error(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::VAULT_IO, path, err)
}

/// Where the key of the vault in `dir` lives now; `None` when nowhere.
///
/// # Errors
///
/// `vault.io` when a key file cannot be read.
pub fn locate(dir: &Path) -> Result<Option<Location>, Error> {
    let keyref = dir.join(KEYREF_FILE);
    match private_fs::read_no_follow(&keyref) {
        Ok(text) => return Ok(Some(Location::Keychain(text.trim().to_owned()))),
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(io_error(&keyref, &err)),
    }
    let file = dir.join(KEY_FILE);
    Ok(std::fs::symlink_metadata(&file)
        .is_ok()
        .then_some(Location::File(file)))
}

/// The key at `location`; `None` when it is not there.
///
/// # Errors
///
/// `vault.io`, a key file that is not a key; keychain errors.
pub fn read(location: &Location, keychain: &dyn Keychain) -> Result<Option<Key>, Error> {
    let text = match location {
        Location::File(path) => match private_fs::read_no_follow(path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_error(path, &err)),
        },
        Location::Keychain(account) => match keychain.get(account)? {
            Some(text) => text,
            None => return Ok(None),
        },
    };
    Key::parse(&text).map(Some).ok_or_else(|| {
        Error::new(
            &codes::VAULT_CORRUPT,
            format!("the vault key in the {} is not a key", location.kind()),
        )
    })
}

/// Writes `key` to `location` (a file: 0600, replacing any older one).
///
/// # Errors
///
/// `vault.io`; keychain errors.
pub fn write(location: &Location, key: &Key, keychain: &dyn Keychain) -> Result<(), Error> {
    match location {
        Location::File(path) => {
            private_fs::write_atomic(path, key.hex().as_bytes()).map_err(|e| io_error(path, &e))
        }
        Location::Keychain(account) => keychain.set(account, &key.hex()),
    }
}

/// Points the vault in `dir` at `location` (the keyref decides).
///
/// # Errors
///
/// `vault.io`.
pub fn select(dir: &Path, location: &Location) -> Result<(), Error> {
    let keyref = dir.join(KEYREF_FILE);
    match location {
        Location::Keychain(account) => {
            let mut text = account.clone();
            text.push('\n');
            private_fs::write_atomic(&keyref, text.as_bytes()).map_err(|e| io_error(&keyref, &e))
        }
        Location::File(_) => match std::fs::remove_file(&keyref) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_error(&keyref, &err)),
        },
    }
}

/// Removes the copy of the key at `location`.
///
/// # Errors
///
/// `vault.io`; keychain errors.
pub fn remove(location: &Location, keychain: &dyn Keychain) -> Result<(), Error> {
    match location {
        Location::File(path) => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_error(path, &err)),
        },
        Location::Keychain(account) => keychain.delete(account).map(drop),
    }
}

/// Creates the key file of a new vault (never replacing one).
///
/// # Errors
///
/// `vault.io`.
pub fn create_file(path: &Path, key: &Key) -> Result<(), Error> {
    private_fs::new_file_options()
        .open(path)
        .and_then(|mut file| file.write_all(key.hex().as_bytes()))
        .map_err(|e| io_error(path, &e))
}

/// Whether the OS keychain answers (a probe read).
#[must_use]
pub fn reachable(keychain: &dyn Keychain, dir: &Path) -> bool {
    keychain.get(&account(dir)).is_ok()
}
