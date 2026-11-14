//! Where a connection's secrets live: the device credential of a `remote`
//! connection in the OS keychain (one entry per connection, never in the
//! registry file, never sent to the webview), and the SHA-256 public-key
//! pins the user confirmed in `pins.json` beside the registry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::secrets::keychain::Keychain;

/// The keychain account of connection `id`'s device credential.
fn account(id: &str) -> String {
    format!("desktop-connection/{id}")
}

/// The keychain behind the registry.
#[derive(Debug, Clone)]
pub struct Credentials {
    keychain: Arc<dyn Keychain>,
}

impl Credentials {
    /// Credentials kept in `keychain` (the OS keychain in the app).
    #[must_use]
    pub fn new(keychain: Arc<dyn Keychain>) -> Self {
        Self { keychain }
    }

    /// The device credential of connection `id`.
    ///
    /// # Errors
    ///
    /// The keychain is unreachable.
    pub fn token(&self, id: &str) -> Result<Option<String>, String> {
        self.keychain.get(&account(id)).map_err(|e| e.to_string())
    }

    /// Keeps `token` for connection `id`.
    ///
    /// # Errors
    ///
    /// The keychain refused it.
    pub fn set_token(&self, id: &str, token: &str) -> Result<(), String> {
        self.keychain
            .set(&account(id), token)
            .map_err(|e| e.to_string())
    }

    /// Forgets connection `id`'s credential.
    pub fn forget(&self, id: &str) {
        if let Err(e) = self.keychain.delete(&account(id)) {
            tracing::warn!(error = %e, "could not delete a connection credential");
        }
    }
}

/// The certificate pins the user confirmed, by connection id.
#[derive(Debug, Clone)]
pub struct Pins {
    file: Option<PathBuf>,
}

impl Pins {
    /// Pins kept in `dir/pins.json` (none without a directory).
    #[must_use]
    pub fn at(dir: Option<&Path>) -> Self {
        Self {
            file: dir.map(|d| d.join("pins.json")),
        }
    }

    fn read(&self) -> BTreeMap<String, String> {
        self.file
            .as_ref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Connection `id`'s pin.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<String> {
        self.read().remove(id)
    }

    /// Pins `fingerprint` for `id` (`None` removes the pin).
    ///
    /// # Errors
    ///
    /// The file could not be written.
    pub fn set(&self, id: &str, fingerprint: Option<&str>) -> Result<(), String> {
        let Some(file) = &self.file else {
            return Err("this shell keeps no certificate pins".to_owned());
        };
        let mut pins = self.read();
        match fingerprint {
            Some(f) => {
                pins.insert(id.to_owned(), f.to_owned());
            }
            None => {
                pins.remove(id);
            }
        }
        let bytes = serde_json::to_vec_pretty(&pins).map_err(|e| e.to_string())?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        pmt_core::private_fs::write_atomic(file, &bytes).map_err(|e| e.to_string())
    }
}
