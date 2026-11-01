//! The `file` backend: `<profile>/secrets.toml`, one `NAME = "value"` pair
//! per secret (spec: security-boundaries, Secrets storage). The file is
//! 0600, never followed through a symlink, and every change is a
//! read-modify-write under an exclusive lock on `secrets.toml.lock`
//! followed by an atomic replace, so two `prometheus secrets set` runs
//! never lose each other's write.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_core::secret::SecretString;

use crate::schema::is_secret_name;

const HEADER: &str = "\
# Prometheus secrets of this profile (mode 0600). Manage them with
# `prometheus secrets set|list|unset`; never commit or share this file.
";

/// `<profile>/secrets.toml`.
#[derive(Debug, Clone)]
pub(crate) struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Every stored secret. A missing file is an empty store.
    pub(crate) fn load(&self) -> Result<BTreeMap<String, SecretString>, Error> {
        let text = match private_fs::read_no_follow(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(store_error(&self.path, &e.to_string())),
        };
        parse(&self.path, &text)
    }

    /// Applies `change` to the stored secrets under the store lock and
    /// writes the result atomically; returns what `change` returned.
    pub(crate) fn update<T>(
        &self,
        change: impl FnOnce(&mut BTreeMap<String, SecretString>) -> T,
    ) -> Result<T, Error> {
        let _lock = self.lock()?;
        let mut secrets = self.load()?;
        let result = change(&mut secrets);
        private_fs::write_atomic(&self.path, render(&secrets).as_bytes())
            .map_err(|e| store_error(&self.path, &e.to_string()))?;
        Ok(result)
    }

    fn lock(&self) -> Result<File, Error> {
        private_fs::lock_beside(&self.path).map_err(|e| store_error(&self.path, &e.to_string()))
    }
}

fn parse(path: &Path, text: &str) -> Result<BTreeMap<String, SecretString>, Error> {
    // The TOML error text can quote the offending line, which may hold a
    // value; report only its position.
    let table: toml::Table = toml::from_str(text).map_err(|e| {
        let at = e
            .span()
            .map(|span| format!(" at byte {}", span.start))
            .unwrap_or_default();
        store_error(path, &format!("is not valid TOML{at}"))
    })?;
    let mut secrets = BTreeMap::new();
    for (name, value) in table {
        if !is_secret_name(&name) {
            return Err(store_error(
                path,
                &format!("'{name}' is not a valid secret name"),
            ));
        }
        let toml::Value::String(value) = value else {
            return Err(store_error(
                path,
                &format!("the value of {name} is not a string"),
            ));
        };
        let value = super::clean_value(&name, value);
        secrets.insert(name, value);
    }
    Ok(secrets)
}

fn render(secrets: &BTreeMap<String, SecretString>) -> String {
    let table: toml::Table = secrets
        .iter()
        .map(|(name, value)| (name.clone(), toml::Value::String(value.expose().to_owned())))
        .collect();
    let body = toml::to_string(&table).expect("a string table serializes to TOML");
    format!("{HEADER}{body}")
}

fn store_error(path: &Path, problem: &str) -> Error {
    Error::new(
        &codes::SECRET_STORE,
        format!("{}: {problem}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use pmt_core::secret::SecretString;

    use super::FileStore;

    #[test]
    fn round_trip_keeps_values_private() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("secrets.toml"));
        assert!(store.load().unwrap().is_empty());
        store
            .update(|s| s.insert("TOKEN".into(), SecretString::new("a \"quoted\" value")))
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded["TOKEN"].expose(), "a \"quoted\" value");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn parse_errors_never_quote_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "TOKEN = \"sk-live-abcdef\nOTHER = 1\n").unwrap();
        let err = FileStore::new(path).load().unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert!(!err.message().contains("sk-live"), "{}", err.message());
    }

    #[test]
    fn non_ascii_is_stripped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "TOKEN = \"abc\u{200b}def\u{a0}\"\n").unwrap();
        assert_eq!(
            FileStore::new(path).load().unwrap()["TOKEN"].expose(),
            "abcdef"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_store_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.toml");
        std::fs::write(&target, "TOKEN = \"x\"\n").unwrap();
        let path = dir.path().join("secrets.toml");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(FileStore::new(path).load().is_err());
    }
}
