//! The administrator-managed layer (spec: configuration-and-profiles,
//! Managed configuration layer; Managed secrets and managed visibility):
//! `managed.toml` in the machine-wide directory (`/etc/prometheus`, or
//! `%ProgramData%\Prometheus` on Windows) holds configuration values that
//! override every profile's own, validated with the same schema, plus a
//! `locked = [...]` list of keys no writer may change; `managed-secrets.toml`
//! beside it (mode 0600 or 0640, owned by root) holds secrets that take
//! precedence over every profile store and the environment.
//!
//! The directory is fixed per platform: no per-user setting or environment
//! variable moves it. Debug builds alone accept a test directory (from
//! `PMT_TEST_MANAGED_DIR`, read once with the launch environment) whose
//! files must then belong to the current user instead of root, so the
//! suite can exercise the layer without writing to `/etc`.
//!
//! The layer is machine-wide, so it is loaded once per process ([`init`])
//! and read by every profile's configuration loading and every writer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use toml_edit::{DocumentMut, Item, Value};

use crate::catalog;

/// The managed configuration file.
pub const MANAGED_FILE: &str = "managed.toml";
/// The managed secrets file.
pub const MANAGED_SECRETS_FILE: &str = "managed-secrets.toml";

/// The machine-wide managed directory of this platform:
/// `%ProgramData%\Prometheus`, with `%ProgramData%` read through the
/// Known Folder API (the system's registered location, not the per-user
/// environment variable a user could point elsewhere to escape the
/// managed layer).
#[cfg(windows)]
#[must_use]
pub fn platform_dir() -> PathBuf {
    known_folders::get_known_folder_path(known_folders::KnownFolder::ProgramData)
        // The API answers on every supported Windows; this is its default.
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("Prometheus")
}

/// The machine-wide managed directory of this platform.
#[cfg(not(windows))]
#[must_use]
pub fn platform_dir() -> PathBuf {
    PathBuf::from("/etc/prometheus")
}

/// The loaded layer.
#[derive(Debug, Default)]
pub struct Managed {
    dir: PathBuf,
    config_loaded: bool,
    values: Vec<(String, Value)>,
    locked: Vec<String>,
    secrets: Option<BTreeMap<String, SecretString>>,
}

/// Who must own the managed secrets file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// root (the platform directory).
    Root,
    /// The current user (a debug build's test directory).
    CurrentUser,
}

static CURRENT: OnceLock<Result<Managed, (&'static pmt_core::error::ErrorCode, String)>> =
    OnceLock::new();

/// Loads the layer from `dir` for the rest of the process; a second call
/// keeps the first result. A layer that does not load makes every
/// configuration load fail closed with its error.
pub fn init(dir: &Path, owner: Owner) {
    let loaded = Managed::load(dir, owner).map_err(|e| (e.code(), e.message().to_owned()));
    let _ = CURRENT.set(loaded);
}

/// The process's layer: `None` when [`init`] never ran (library callers
/// and tests).
///
/// # Errors
///
/// `config.managed_invalid` (78) when the layer did not load.
pub fn current() -> Result<Option<&'static Managed>, Error> {
    match CURRENT.get() {
        None => Ok(None),
        Some(Ok(managed)) => Ok(Some(managed)),
        Some(Err((code, message))) => Err(Error::new(code, message.clone())
            .with_hint("the administrator must fix the managed configuration")),
    }
}

/// `config key '<key>' is managed by your administrator` (exit 77).
#[must_use]
pub fn locked_error(key: &str) -> Error {
    Error::new(
        &codes::CONFIG_MANAGED_KEY,
        format!("config key '{key}' is managed by your administrator"),
    )
}

/// Fails when the process's layer locks `key` (or a key under it, or the
/// section it is in).
///
/// # Errors
///
/// `config.managed_key` (77); the layer's own load error.
pub fn check_writable(key: &str) -> Result<(), Error> {
    match current()? {
        Some(managed) if managed.locks(key) => Err(locked_error(key)),
        _ => Ok(()),
    }
}

/// Fails when `name` is an administrator-managed secret, which
/// `prometheus secrets` may neither set nor unset.
///
/// # Errors
///
/// `secret.managed` (77); the layer's own load error.
pub fn check_secret_writable(name: &str) -> Result<(), Error> {
    match current()? {
        Some(managed) if managed.secret(name).is_some() => Err(Error::new(
            &codes::SECRET_MANAGED,
            format!("secret '{name}' is managed by your administrator"),
        )),
        _ => Ok(()),
    }
}

impl Managed {
    /// Reads `dir`'s managed files; absent files leave their part empty.
    ///
    /// # Errors
    ///
    /// `config.managed_invalid` for a file that does not parse or validate,
    /// a locked key the schema does not know, or a secrets file with loose
    /// permissions or the wrong owner.
    pub fn load(dir: &Path, owner: Owner) -> Result<Self, Error> {
        let mut managed = Self {
            dir: dir.to_owned(),
            ..Self::default()
        };
        let path = dir.join(MANAGED_FILE);
        if let Some(text) = read(&path)? {
            managed.config_loaded = true;
            managed.parse_config(&path, &text)?;
        }
        let secrets = dir.join(MANAGED_SECRETS_FILE);
        if let Some(text) = read(&secrets)? {
            check_secrets_file(&secrets, owner)?;
            let parsed = crate::secrets::file::parse(&secrets, &text)
                .map_err(|e| invalid(&secrets, e.message()))?;
            managed.secrets = Some(parsed);
        }
        Ok(managed)
    }

    fn parse_config(&mut self, path: &Path, text: &str) -> Result<(), Error> {
        let mut doc: DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| invalid(path, e.message()))?;
        if let Some(item) = doc.remove("locked") {
            let list = item
                .as_array()
                .ok_or_else(|| invalid(path, "`locked` must be an array of keys"))?;
            for key in list {
                let key = key
                    .as_str()
                    .ok_or_else(|| invalid(path, "`locked` must be an array of keys"))?;
                if catalog::lookup(key).is_none() {
                    return Err(invalid(
                        path,
                        &format!("`locked` names unknown key '{key}'"),
                    ));
                }
                self.locked.push(key.to_owned());
            }
        }
        let rest = doc.to_string();
        crate::parse::config(path, &rest).map_err(|e| invalid(path, e.message()))?;
        collect(doc.as_item(), "", &mut self.values);
        Ok(())
    }

    /// The managed directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `managed.toml`.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.dir.join(MANAGED_FILE)
    }

    /// `managed-secrets.toml`.
    #[must_use]
    pub fn secrets_path(&self) -> PathBuf {
        self.dir.join(MANAGED_SECRETS_FILE)
    }

    /// Whether `managed.toml` exists (and loaded).
    #[must_use]
    pub fn config_loaded(&self) -> bool {
        self.config_loaded
    }

    /// Whether `managed-secrets.toml` exists (and loaded).
    #[must_use]
    pub fn secrets_loaded(&self) -> bool {
        self.secrets.is_some()
    }

    /// The managed values, as dotted keys.
    #[must_use]
    pub fn values(&self) -> &[(String, Value)] {
        &self.values
    }

    /// The locked keys.
    #[must_use]
    pub fn locked(&self) -> &[String] {
        &self.locked
    }

    /// Whether a writer changing `key` would touch a locked key: the key
    /// itself, a section containing a locked key, or a key inside a locked
    /// section.
    #[must_use]
    pub fn locks(&self, key: &str) -> bool {
        self.locked.iter().any(|locked| {
            locked == key
                || key.starts_with(&format!("{locked}."))
                || locked.starts_with(&format!("{key}."))
        })
    }

    /// The managed value of secret `name`.
    #[must_use]
    pub fn secret(&self, name: &str) -> Option<&SecretString> {
        self.secrets.as_ref()?.get(name)
    }

    /// Every managed secret name.
    #[must_use]
    pub fn secret_names(&self) -> Vec<String> {
        self.secrets
            .as_ref()
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Every managed secret (for redaction).
    pub fn secrets(&self) -> impl Iterator<Item = (&String, &SecretString)> {
        self.secrets.iter().flatten()
    }
}

fn read(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(invalid(path, &format!("cannot read it: {e}"))),
    }
}

fn invalid(path: &Path, why: &str) -> Error {
    Error::new(
        &codes::CONFIG_MANAGED_INVALID,
        format!("{}: {why}", path.display()),
    )
}

/// Leaf values of `item` as dotted keys (tables recursed, arrays kept).
fn collect(item: &Item, prefix: &str, out: &mut Vec<(String, Value)>) {
    match item {
        Item::Table(table) => {
            for (name, child) in table {
                collect(child, &join(prefix, name), out);
            }
        }
        Item::Value(Value::InlineTable(table)) => {
            for (name, child) in table {
                collect(&Item::Value(child.clone()), &join(prefix, name), out);
            }
        }
        Item::Value(value) => out.push((prefix.to_owned(), value.clone())),
        Item::ArrayOfTables(_) => {
            if let Ok(value) = item.clone().into_value() {
                out.push((prefix.to_owned(), value));
            }
        }
        Item::None => {}
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

/// The secrets file must not be readable by others and must belong to
/// root (or, for a test directory, to the current user).
#[cfg(unix)]
fn check_secrets_file(path: &Path, owner: Owner) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).map_err(|e| invalid(path, &e.to_string()))?;
    let mode = meta.mode() & 0o777;
    if mode != 0o600 && mode != 0o640 {
        return Err(invalid(
            path,
            &format!("mode is {mode:04o}; it must be 0600 or 0640"),
        ));
    }
    let expected = match owner {
        Owner::Root => 0,
        Owner::CurrentUser => nix::unistd::getuid().as_raw(),
    };
    if meta.uid() != expected {
        return Err(invalid(path, "it must be owned by root"));
    }
    Ok(())
}

/// `%ProgramData%` is writable only by administrators by default; the
/// file's own ACL is the administrator's responsibility.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // the same signature as the unix check
fn check_secrets_file(_path: &Path, _owner: Owner) -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
mod tests;
