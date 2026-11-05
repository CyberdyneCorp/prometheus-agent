//! The encrypted vault file `<profile>/vault/vault.enc` (spec:
//! security-boundaries, Credential vault storage): `PMTVAULT1`, a random
//! 24-byte nonce and the XChaCha20-Poly1305 sealing of the items' JSON,
//! written atomically (0600) under the vault's lock.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chacha20poly1305::aead::{Aead as _, Payload};
use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305, XNonce};
use pmt_config::schema::VaultKeyStorage;
use pmt_config::secrets::Keychain;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::items::{Item, Meta};
use crate::key::{self, Key, Location};

/// The vault file name.
pub const VAULT_FILE: &str = "vault.enc";
/// The vault directory, relative to the profile directory.
pub const DIR: &str = "vault";

const MAGIC: &[u8] = b"PMTVAULT1";
const AAD: &[u8] = b"prometheus-vault-v1";
const NONCE_LEN: usize = 24;
const SCHEMA: &str = "prometheus.vault.v1";

#[derive(Serialize, Deserialize)]
struct Contents {
    schema: String,
    items: Vec<Item>,
}

/// One profile's vault.
pub struct Vault {
    dir: PathBuf,
    keychain: Arc<dyn Keychain>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

fn io_error(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::VAULT_IO, path, err)
}

fn corrupt() -> Error {
    Error::new(
        &codes::VAULT_CORRUPT,
        "vault.enc cannot be decrypted with the vault key (wrong key or a damaged file)",
    )
}

impl Vault {
    /// The vault of the profile at `profile_dir`; nothing is read or
    /// created until it is used.
    #[must_use]
    pub fn of(profile_dir: &Path, keychain: Arc<dyn Keychain>) -> Self {
        Self {
            dir: profile_dir.join(DIR),
            keychain,
        }
    }

    /// `<profile>/vault`.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self) -> PathBuf {
        self.dir.join(VAULT_FILE)
    }

    /// Whether `vault.enc` exists.
    #[must_use]
    pub fn exists(&self) -> bool {
        std::fs::symlink_metadata(self.file()).is_ok()
    }

    /// Where the key lives now.
    ///
    /// # Errors
    ///
    /// `vault.io`.
    pub fn key_location(&self) -> Result<Option<Location>, Error> {
        key::locate(&self.dir)
    }

    /// Whether the OS keychain answers.
    #[must_use]
    pub fn keychain_reachable(&self) -> bool {
        key::reachable(self.keychain.as_ref(), &self.dir)
    }

    /// The key of an existing vault.
    fn key(&self) -> Result<Key, Error> {
        let location = self.key_location()?;
        let key = match &location {
            Some(location) => key::read(location, self.keychain.as_ref())?,
            None => None,
        };
        key.ok_or_else(|| Error::new(&codes::VAULT_KEY_NOT_FOUND, key::KEY_NOT_FOUND))
    }

    /// The items; an empty list when there is no vault yet.
    ///
    /// # Errors
    ///
    /// `vault.key_not_found`, `vault.corrupt`, `vault.io`.
    pub fn items(&self) -> Result<Vec<Item>, Error> {
        let file = self.file();
        let sealed = match std::fs::read(&file) {
            Ok(sealed) => sealed,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(io_error(&file, &err)),
        };
        open(&self.key()?, &sealed)
    }

    /// Every item's metadata.
    ///
    /// # Errors
    ///
    /// As [`Vault::items`].
    pub fn list(&self) -> Result<Vec<Meta>, Error> {
        Ok(self.items()?.iter().map(Item::meta).collect())
    }

    /// The item of `handle`.
    ///
    /// # Errors
    ///
    /// `vault.not_found`; as [`Vault::items`].
    pub fn get(&self, handle: &str) -> Result<Item, Error> {
        self.items()?
            .into_iter()
            .find(|i| i.handle == handle)
            .ok_or_else(|| not_found(handle))
    }

    /// Adds `item` (creating the vault and its key on first use).
    ///
    /// # Errors
    ///
    /// As [`Vault::update`].
    pub fn add(&self, item: Item, storage: VaultKeyStorage) -> Result<Meta, Error> {
        let meta = item.meta();
        self.update(storage, |items| {
            items.push(item);
            Ok(())
        })?;
        Ok(meta)
    }

    /// Removes the item of `handle`.
    ///
    /// # Errors
    ///
    /// `vault.not_found`; as [`Vault::update`].
    pub fn remove(&self, handle: &str) -> Result<Meta, Error> {
        let mut removed = None;
        self.update(VaultKeyStorage::File, |items| {
            let at = items
                .iter()
                .position(|i| i.handle == handle)
                .ok_or_else(|| not_found(handle))?;
            removed = Some(items.remove(at).meta());
            Ok(())
        })?;
        removed.ok_or_else(|| not_found(handle))
    }

    /// Reads, changes and writes the items under the vault's lock; a new
    /// vault gets a key stored as `storage` asks.
    ///
    /// # Errors
    ///
    /// `vault.key_not_found` (never re-keyed), `vault.corrupt`,
    /// `vault.io`; what `change` returns.
    pub fn update(
        &self,
        storage: VaultKeyStorage,
        change: impl FnOnce(&mut Vec<Item>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        private_fs::create_dir(&self.dir).or_else(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(io_error(&self.dir, &e))
            }
        })?;
        let file = self.file();
        let _lock = private_fs::lock_beside(&file).map_err(|e| io_error(&file, &e))?;
        let sealed = if self.exists() {
            let (key, mut items) = (self.key()?, self.items()?);
            change(&mut items)?;
            seal(&key, &items)
        } else {
            // The change runs first so one that fails (removing from an
            // empty vault) creates no key: a key left behind would be
            // reused later even when `storage` asks for the keychain.
            let mut items = Vec::new();
            change(&mut items)?;
            seal(&self.new_key(storage)?, &items)
        };
        private_fs::write_atomic(&file, &sealed).map_err(|e| io_error(&file, &e))
    }

    /// The key of a new vault: in the keychain when asked and reachable
    /// (else the file, with a WARN), else in `vault.key`. A key left from a
    /// deleted vault is reused, never silently replaced.
    fn new_key(&self, storage: VaultKeyStorage) -> Result<Key, Error> {
        if let Some(location) = self.key_location()?
            && let Some(key) = key::read(&location, self.keychain.as_ref())?
        {
            return Ok(key);
        }
        let key = Key::generate();
        if storage == VaultKeyStorage::Keychain {
            let location = Location::Keychain(key::account(&self.dir));
            match key::write(&location, &key, self.keychain.as_ref()) {
                Ok(()) => {
                    key::select(&self.dir, &location)?;
                    return Ok(key);
                }
                Err(err) => tracing::warn!(
                    "vault.key_storage = \"keychain\" but no keychain is reachable ({}); the new vault's key is kept in vault.key",
                    err.message()
                ),
            }
        }
        key::create_file(&self.dir.join(key::KEY_FILE), &key)?;
        Ok(key)
    }

    /// Moves the key to `target`: writes it there, checks that it decrypts
    /// the vault, points the vault at it and only then removes the old
    /// copy. Running it again after an interruption finishes the move.
    ///
    /// # Errors
    ///
    /// `vault.key_not_found`, `vault.corrupt` (the written copy does not
    /// decrypt: nothing is switched), `vault.io`; keychain errors.
    pub fn migrate_key(&self, target: VaultKeyStorage) -> Result<Location, Error> {
        let file = self.file();
        let _lock = private_fs::lock_beside(&file).map_err(|e| io_error(&file, &e))?;
        let current = self
            .key_location()?
            .ok_or_else(|| Error::new(&codes::VAULT_KEY_NOT_FOUND, key::KEY_NOT_FOUND))?;
        let key = self.key()?;
        let destination = match target {
            VaultKeyStorage::File => Location::File(self.dir.join(key::KEY_FILE)),
            VaultKeyStorage::Keychain => Location::Keychain(key::account(&self.dir)),
        };
        let keychain = self.keychain.as_ref();
        key::write(&destination, &key, keychain)?;
        let written = key::read(&destination, keychain)?.ok_or_else(corrupt)?;
        if let Ok(sealed) = std::fs::read(&file) {
            open(&written, &sealed)?;
        }
        key::select(&self.dir, &destination)?;
        if current != destination {
            key::remove(&current, keychain)?;
        }
        // A copy an interrupted earlier move left behind (best effort: the
        // move itself is complete).
        let stale = match &destination {
            Location::File(_) => Location::Keychain(key::account(&self.dir)),
            Location::Keychain(_) => Location::File(self.dir.join(key::KEY_FILE)),
        };
        if let Err(err) = key::remove(&stale, keychain) {
            tracing::warn!(
                "an old copy of the vault key was not removed: {}",
                err.message()
            );
        }
        Ok(destination)
    }
}

fn not_found(handle: &str) -> Error {
    Error::new(&codes::VAULT_NOT_FOUND, format!("no vault item '{handle}'"))
        .with_hint("prometheus vault list shows the handles")
}

fn seal(key: &Key, items: &[Item]) -> Vec<u8> {
    let contents = Contents {
        schema: SCHEMA.to_owned(),
        items: items.to_vec(),
    };
    let json = serde_json::to_vec(&contents).expect("vault items serialize");
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).expect("the OS random generator is available");
    let cipher = XChaCha20Poly1305::new(&key.0.into());
    let sealed = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &json,
                aad: AAD,
            },
        )
        .expect("encryption of a bounded buffer succeeds");
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + sealed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    out
}

fn open(key: &Key, sealed: &[u8]) -> Result<Vec<Item>, Error> {
    let body = sealed.strip_prefix(MAGIC).ok_or_else(corrupt)?;
    if body.len() < NONCE_LEN {
        return Err(corrupt());
    }
    let (nonce, ciphertext) = body.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| corrupt())?;
    let cipher = XChaCha20Poly1305::new(&key.0.into());
    let json = cipher
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: ciphertext,
                aad: AAD,
            },
        )
        .map_err(|_| corrupt())?;
    let contents: Contents = serde_json::from_slice(&json).map_err(|_| corrupt())?;
    Ok(contents.items)
}
