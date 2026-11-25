//! The crypto store (spec: platform-adapters, Matrix end-to-end
//! encryption): `<profile>/platforms/matrix/crypto/` (mode 0700) holds
//! `store.json`, the Olm account, Olm sessions and Megolm sessions as
//! vodozemac pickles encrypted with a 32-byte key kept in the profile's
//! secrets store (`MATRIX_CRYPTO_STORE_KEY`), so the files alone reveal
//! no key. Written atomically after every change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use pmt_config::ProfileScope;
use pmt_core::secret::SecretString;
use serde::{Deserialize, Serialize};

/// The secret holding the store's pickle key.
pub(crate) const KEY_SECRET: &str = "MATRIX_CRYPTO_STORE_KEY";
const FILE: &str = "store.json";

/// What `store.json` holds.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Persisted {
    pub(crate) user_id: String,
    pub(crate) device_id: String,
    /// The encrypted account pickle.
    pub(crate) account: String,
    /// The device keys were uploaded.
    pub(crate) uploaded: bool,
    /// Olm sessions by the other device's Curve25519 key.
    pub(crate) sessions: BTreeMap<String, Vec<String>>,
    /// Megolm sessions others share, by `<room> <session id>`.
    pub(crate) inbound: BTreeMap<String, Inbound>,
    /// The Megolm session this device sends with, by room.
    pub(crate) outbound: BTreeMap<String, Outbound>,
}

/// A Megolm session another device shares.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Inbound {
    pub(crate) room_id: String,
    /// The sending device's Curve25519 key.
    pub(crate) sender_key: String,
    /// The user who shared it: the only sender its events may name.
    #[serde(default)]
    pub(crate) sender: String,
    /// The sharing device's Ed25519 key (checked against the user's
    /// device list when the key arrived).
    #[serde(default)]
    pub(crate) ed25519: String,
    pub(crate) pickle: String,
}

/// The Megolm session of a room this device sends with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Outbound {
    pub(crate) pickle: String,
    /// The devices it was shared with: (user, device).
    pub(crate) shared: Vec<(String, String)>,
    /// Messages encrypted with it.
    pub(crate) messages: u32,
    /// When it was created (milliseconds since the epoch).
    pub(crate) created_ms: i64,
}

/// Where the store lives and the key of its pickles.
pub(crate) struct Store {
    dir: PathBuf,
    pub(crate) key: [u8; 32],
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// The store directory of `scope`.
pub(crate) fn dir_of(scope: &ProfileScope) -> PathBuf {
    scope.path("platforms/matrix/crypto")
}

impl Store {
    /// Opens the store of `scope`: its directory (created 0700) and its key
    /// (created with the store; a store without its key cannot be read).
    ///
    /// # Errors
    ///
    /// Why the store cannot be used, for the operator.
    pub(crate) fn open(scope: &ProfileScope) -> Result<(Self, Option<Persisted>), String> {
        let dir = dir_of(scope);
        pmt_core::private_fs::create_dir(&dir)
            .and_then(|()| pmt_core::private_fs::restrict_to_owner(&dir))
            .map_err(|e| format!("the crypto store {} cannot be created: {e}", dir.display()))?;
        let file = dir.join(FILE);
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(format!(
                    "the crypto store {} cannot be read: {e}",
                    file.display()
                ));
            }
        };
        let secrets = scope.secrets().store();
        let stored = secrets
            .get(KEY_SECRET)
            .map_err(|e| format!("the secret {KEY_SECRET} cannot be read: {}", e.message()))?;
        let key = match (stored, &text) {
            (Some(key), _) => decode_key(key.expose())?,
            (None, Some(_)) => {
                return Err(format!(
                    "the crypto store {} exists but its key secret {KEY_SECRET} is missing",
                    file.display()
                ));
            }
            (None, None) => {
                let key = new_key()?;
                let encoded = base64::engine::general_purpose::STANDARD.encode(key);
                secrets
                    .set(KEY_SECRET, &SecretString::new(encoded))
                    .map_err(|e| {
                        format!("the secret {KEY_SECRET} cannot be stored: {}", e.message())
                    })?;
                key
            }
        };
        let persisted = text
            .map(|text| {
                serde_json::from_str::<Persisted>(&text)
                    .map_err(|e| format!("the crypto store {} is corrupted: {e}", file.display()))
            })
            .transpose()?;
        Ok((Self { dir, key }, persisted))
    }

    /// The store's directory.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes `persisted` (atomically).
    pub(crate) fn save(&self, persisted: &Persisted) -> Result<(), String> {
        let text = serde_json::to_vec(persisted).map_err(|e| e.to_string())?;
        pmt_core::private_fs::write_atomic(&self.dir.join(FILE), &text)
            .map_err(|e| format!("the crypto store cannot be written: {e}"))
    }
}

fn decode_key(text: &str) -> Result<[u8; 32], String> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| format!("the secret {KEY_SECRET} is not a 32-byte base64 key"))
}

fn new_key() -> Result<[u8; 32], String> {
    let mut key = [0; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key)
        .map_err(|_| "no randomness for the crypto store key".to_owned())?;
    Ok(key)
}
