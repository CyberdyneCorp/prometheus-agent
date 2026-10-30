//! The `keychain` backend: one OS keychain entry per secret under service
//! `prometheus` and account `<home-id>/<profile>/<NAME>` (spec:
//! configuration-and-profiles, Secrets store).
//!
//! Keychains cannot enumerate entries portably, so the backend also keeps
//! the profile's secret names (never values) in one more entry, account
//! `<home-id>/<profile>/.names`. The home id is derived from the home path,
//! so two homes on one machine never share entries.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use sha2::{Digest as _, Sha256};

/// Keychain service name of every Prometheus entry.
pub const SERVICE: &str = "prometheus";
const NAMES_ENTRY: &str = ".names";

/// A credential store keyed by account (service [`SERVICE`]).
pub trait Keychain: Send + Sync + std::fmt::Debug {
    /// The value of `account`, `None` when there is no entry.
    ///
    /// # Errors
    ///
    /// The keychain is unreachable or refused the read.
    fn get(&self, account: &str) -> Result<Option<String>, Error>;
    /// Creates or replaces `account`.
    ///
    /// # Errors
    ///
    /// The keychain is unreachable or refused the write.
    fn set(&self, account: &str, value: &str) -> Result<(), Error>;
    /// Deletes `account`; `false` when there was no entry.
    ///
    /// # Errors
    ///
    /// The keychain is unreachable or refused the delete.
    fn delete(&self, account: &str) -> Result<bool, Error>;
}

/// The platform keychain: macOS Keychain, Windows Credential Manager, or
/// the Secret Service (over libdbus) on other Unix systems.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsKeychain;

type Store = Arc<keyring_core::CredentialStore>;

/// The platform store, connected once per process on first use (a
/// connection to the OS service, not profile state).
static NATIVE: LazyLock<Result<Store, String>> =
    LazyLock::new(|| native_store().map_err(|e| e.to_string()));

fn native_store() -> keyring_core::Result<Store> {
    #[cfg(target_os = "macos")]
    let store: Store = apple_native_keyring_store::keychain::Store::new()?;
    #[cfg(windows)]
    let store: Store = windows_native_keyring_store::Store::new()?;
    #[cfg(all(unix, not(target_os = "macos")))]
    let store: Store = dbus_secret_service_keyring_store::Store::new()?;
    #[cfg(not(any(unix, windows)))]
    let store: Store = return Err(keyring_core::Error::NotSupportedByStore(
        "no OS keychain on this platform".to_owned(),
    ));
    Ok(store)
}

impl OsKeychain {
    fn entry(account: &str) -> Result<keyring_core::Entry, Error> {
        let store = NATIVE.as_ref().map_err(|e| unavailable(e))?;
        store
            .build(SERVICE, account, None)
            .map_err(|e| keychain_error(&e))
    }
}

impl Keychain for OsKeychain {
    fn get(&self, account: &str) -> Result<Option<String>, Error> {
        match Self::entry(account)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(keychain_error(&e)),
        }
    }

    fn set(&self, account: &str, value: &str) -> Result<(), Error> {
        Self::entry(account)?
            .set_password(value)
            .map_err(|e| keychain_error(&e))
    }

    fn delete(&self, account: &str) -> Result<bool, Error> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(e) => Err(keychain_error(&e)),
        }
    }
}

fn unavailable(reason: &dyn std::fmt::Display) -> Error {
    Error::new(
        &codes::SECRET_KEYCHAIN_UNAVAILABLE,
        format!("the OS keychain is not available: {reason}"),
    )
    .with_hint("use the file backend: prometheus secrets migrate --to file")
}

fn keychain_error(e: &keyring_core::Error) -> Error {
    match e {
        keyring_core::Error::NoStorageAccess(_)
        | keyring_core::Error::PlatformFailure(_)
        | keyring_core::Error::NoDefaultStore
        | keyring_core::Error::NotSupportedByStore(_) => unavailable(e),
        _ => Error::new(&codes::SECRET_STORE, format!("OS keychain: {e}")),
    }
}

/// An in-memory keychain shared by its clones, for tests and for machines
/// where a test must not touch the real keychain.
#[derive(Debug, Default, Clone)]
pub struct MemoryKeychain {
    entries: Arc<Mutex<BTreeMap<String, String>>>,
}

impl MemoryKeychain {
    /// Every account currently stored (values excluded).
    #[must_use]
    pub fn accounts(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Keychain for MemoryKeychain {
    fn get(&self, account: &str) -> Result<Option<String>, Error> {
        Ok(self.lock().get(account).cloned())
    }

    fn set(&self, account: &str, value: &str) -> Result<(), Error> {
        self.lock().insert(account.to_owned(), value.to_owned());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<bool, Error> {
        Ok(self.lock().remove(account).is_some())
    }
}

/// One profile's secrets in a keychain.
#[derive(Debug, Clone)]
pub(crate) struct KeychainStore {
    prefix: String,
    keychain: Arc<dyn Keychain>,
}

impl KeychainStore {
    pub(crate) fn new(home: &Path, profile: &str, keychain: Arc<dyn Keychain>) -> Self {
        Self {
            prefix: format!("{}/{profile}/", home_id(home)),
            keychain,
        }
    }

    pub(crate) fn get(&self, name: &str) -> Result<Option<SecretString>, Error> {
        let value = self.keychain.get(&self.account(name))?;
        Ok(value.map(|v| super::clean_value(name, v)))
    }

    pub(crate) fn set(&self, name: &str, value: &SecretString) -> Result<(), Error> {
        self.keychain.set(&self.account(name), value.expose())?;
        let mut names = self.names()?;
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
            self.write_names(&names)?;
        }
        Ok(())
    }

    pub(crate) fn remove(&self, name: &str) -> Result<bool, Error> {
        let removed = self.keychain.delete(&self.account(name))?;
        let mut names = self.names()?;
        let before = names.len();
        names.retain(|n| n != name);
        if names.len() != before {
            self.write_names(&names)?;
        }
        Ok(removed)
    }

    pub(crate) fn names(&self) -> Result<Vec<String>, Error> {
        let index = self.keychain.get(&self.account(NAMES_ENTRY))?;
        let mut names: Vec<String> = index
            .unwrap_or_default()
            .lines()
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    fn write_names(&self, names: &[String]) -> Result<(), Error> {
        let account = self.account(NAMES_ENTRY);
        if names.is_empty() {
            self.keychain.delete(&account).map(drop)
        } else {
            self.keychain.set(&account, &names.join("\n"))
        }
    }

    fn account(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }
}

/// A short stable id of the home: the first 16 hex digits of the SHA-256
/// of its path (absolute when it can be made so).
#[must_use]
pub fn home_id(home: &Path) -> String {
    let path = std::path::absolute(home).unwrap_or_else(|_| home.to_owned());
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    digest[..8].iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use pmt_core::secret::SecretString;

    use super::{KeychainStore, MemoryKeychain, home_id};

    #[test]
    fn entries_are_scoped_by_home_and_profile() {
        let keychain = MemoryKeychain::default();
        let work = KeychainStore::new(Path::new("/h1"), "work", Arc::new(keychain.clone()));
        let other = KeychainStore::new(Path::new("/h2"), "work", Arc::new(keychain.clone()));
        work.set("TOKEN", &SecretString::new("v1")).unwrap();
        assert_eq!(work.get("TOKEN").unwrap().unwrap().expose(), "v1");
        assert!(other.get("TOKEN").unwrap().is_none());
        assert_eq!(work.names().unwrap(), ["TOKEN"]);
        let prefix = format!("{}/work/", home_id(Path::new("/h1")));
        assert!(keychain.accounts().iter().all(|a| a.starts_with(&prefix)));
        assert!(work.remove("TOKEN").unwrap());
        assert!(work.names().unwrap().is_empty());
        assert!(keychain.accounts().is_empty());
    }
}
