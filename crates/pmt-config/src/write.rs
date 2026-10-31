//! The one `config.toml` writer (spec: configuration-and-profiles,
//! Format-preserving atomic config writes). Every change is applied to the
//! format-preserving document, the whole result is validated, the previous
//! file is kept under `<profile>/backups/config/` (newest 10), and the new
//! file replaces the old one atomically (temp file, fsync, rename, 0600).
//! A file that does not parse is never overwritten except by
//! [`ConfigFile::replace`], which is how `config edit` repairs it.

use std::io;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use toml_edit::DocumentMut;

use crate::layers::{Layered, Overrides, typed_value};
use crate::migrate::{self, Migrated};
use crate::schema::FallbackEntry;
use crate::{backups, catalog, edit, parse};

/// A profile's `config.toml` with its backup directory.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    path: PathBuf,
    backups: PathBuf,
}

impl ConfigFile {
    pub(crate) fn new(path: PathBuf, backups: PathBuf) -> Self {
        Self { path, backups }
    }

    /// The file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory of write and migration backups.
    #[must_use]
    pub fn backups_dir(&self) -> &Path {
        &self.backups
    }

    /// The file's text at the current schema version, migrating (with a
    /// backup) a file from an older version; a missing file is empty.
    ///
    /// # Errors
    ///
    /// `config.parse`, `config.too_new` or `home.io`.
    pub fn read(&self) -> Result<(String, Option<Migrated>), Error> {
        let text = self.raw()?.unwrap_or_default();
        migrate::upgrade(&self.path, &text, &self.backups)
    }

    /// The file's bytes as they are on disk, without migrating.
    ///
    /// # Errors
    ///
    /// `home.io` when it exists but cannot be read.
    pub fn raw(&self) -> Result<Option<String>, Error> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_error(&self.path, &e)),
        }
    }

    /// Reads, migrates, validates and layers the file under `overrides`.
    ///
    /// # Errors
    ///
    /// As [`ConfigFile::read`], plus validation errors (exit 78) and
    /// invalid overrides (exit 2).
    pub(crate) fn load(&self, overrides: &Overrides) -> Result<Layered, Error> {
        let (text, _) = self.read()?;
        Layered::build(&self.path, &text, overrides)
    }

    /// `config set`: types `raw` by the key's schema (which refuses a
    /// credential for a non-secret key) and writes it.
    ///
    /// # Errors
    ///
    /// `config.invalid_argument` or `config.credential_value` (exit 2) for
    /// the argument; the file's own errors otherwise. Nothing is written on
    /// error.
    pub fn set(&self, key: &str, raw: &str) -> Result<(), Error> {
        let value = typed_value(key, raw)?;
        self.modify(|doc| {
            edit::set(doc, key, value)
                .map_err(|message| Error::new(&codes::CONFIG_INVALID_ARGUMENT, message))
        })
    }

    /// Sets every `(key, raw)` pair in one validated write (all or none).
    ///
    /// # Errors
    ///
    /// As [`set`](Self::set), for the first failing pair.
    pub fn set_all(&self, pairs: &[(&str, &str)]) -> Result<(), Error> {
        let typed = pairs
            .iter()
            .map(|(key, raw)| typed_value(key, raw).map(|value| (*key, value)))
            .collect::<Result<Vec<_>, _>>()?;
        self.modify(|doc| {
            for (key, value) in typed {
                edit::set(doc, key, value)
                    .map_err(|message| Error::new(&codes::CONFIG_INVALID_ARGUMENT, message))?;
            }
            Ok(())
        })
    }

    /// Replaces `[[fallback.chain]]` with `chain` (an empty chain removes
    /// the array), keeping the rest of the file as it is.
    ///
    /// # Errors
    ///
    /// The file's own errors and validation of the result.
    pub fn set_fallback_chain(&self, chain: &[FallbackEntry]) -> Result<(), Error> {
        self.modify(|doc| {
            edit::set_fallback_chain(doc, chain);
            Ok(())
        })
    }

    /// `config unset`: removes `key` so its default applies; returns
    /// whether it was set.
    ///
    /// # Errors
    ///
    /// `config.invalid_argument` (exit 2) for an unknown key; the file's
    /// own errors otherwise.
    pub fn unset(&self, key: &str) -> Result<bool, Error> {
        if catalog::lookup(key).is_none() {
            return Err(Error::new(
                &codes::CONFIG_INVALID_ARGUMENT,
                crate::validate::unknown_key(key, None).message,
            ));
        }
        let mut removed = false;
        self.modify(|doc| {
            removed = edit::remove(doc, key);
            Ok(())
        })?;
        Ok(removed)
    }

    /// Replaces the whole file with `text` (the `config edit` result) after
    /// validating it; works even when the file on disk does not parse.
    ///
    /// # Errors
    ///
    /// The validation errors of `text`, a credential in a non-secret key
    /// included (exit 78), `home.io`.
    pub fn replace(&self, text: &str) -> Result<(), Error> {
        check_document(&self.path, text)?;
        let old = self.raw()?;
        self.commit(old.as_deref(), text)
    }

    /// Applies `change` to the parsed current file, validates and commits.
    fn modify(
        &self,
        change: impl FnOnce(&mut DocumentMut) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let (old, _) = self.read()?;
        let mut doc = parse::document(&self.path, &old)?.into_mut();
        change(&mut doc)?;
        let new = doc.to_string();
        crate::essential::semantic(&parse::config(&self.path, &new)?)?;
        if new == old {
            return Ok(());
        }
        self.commit(Some(&old), &new)
    }

    fn commit(&self, old: Option<&str>, new: &str) -> Result<(), Error> {
        if let Some(old) = old.filter(|old| *old != new) {
            backups::save(&self.backups, "config", old)?;
            backups::prune(&self.backups);
        }
        private_fs::write_atomic(&self.path, new.as_bytes()).map_err(|e| io_error(&self.path, &e))
    }
}

/// Everything a written file must satisfy: current schema version and a
/// valid document (which has no credential in a non-secret key).
pub(crate) fn check_document(path: &Path, text: &str) -> Result<(), Error> {
    let doc = parse::document(path, text)?;
    let version = migrate::file_version(&doc.clone().into_mut())?;
    if version != crate::SCHEMA_VERSION {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            format!(
                "schema_version must be {} (this file says {version})",
                crate::SCHEMA_VERSION
            ),
        ));
    }
    crate::essential::semantic(&parse::config(path, text)?)
}

fn io_error(path: &Path, e: &io::Error) -> Error {
    Error::new(
        &codes::HOME_IO,
        format!("cannot write {}: {e}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::ConfigFile;
    use crate::backups::{KEEP, write_backups};

    fn file(text: Option<&str>) -> (tempfile::TempDir, ConfigFile) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        if let Some(text) = text {
            std::fs::write(&path, text).unwrap();
        }
        let config = ConfigFile::new(path, dir.path().join("backups/config"));
        (dir, config)
    }

    fn contents(config: &ConfigFile) -> String {
        std::fs::read_to_string(config.path()).unwrap()
    }

    #[test]
    fn set_keeps_comments_and_every_other_byte() {
        let text = "schema_version = 1\n\n# the main model\n[model]\ndefault = \"x\" # keep\n\n[display]\nbell_on_complete = true\n";
        let (_dir, config) = file(Some(text));
        config.set("display.theme", "mono").unwrap();
        assert_eq!(contents(&config), format!("{text}theme = \"mono\"\n"));
        let effective = config.load(&crate::Overrides::default()).unwrap();
        assert_eq!(effective.config().display.theme, "mono");
    }

    #[test]
    fn invalid_value_is_never_written() {
        let text = "[agent]\nmax_iterations = 40\n";
        let (_dir, config) = file(Some(text));
        let err = config.set("agent.max_iterations", "-3").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert_eq!(
            err.message(),
            "agent.max_iterations: must be between 0 and 100000"
        );
        assert_eq!(contents(&config), text);
    }

    #[test]
    fn credentials_are_refused_but_secret_names_are_fine() {
        let (_dir, config) = file(Some(""));
        let err = config
            .set("providers.acme.base_url", "sk-live-123456")
            .unwrap_err();
        assert_eq!(err.code().code, "config.credential_value");
        assert_eq!(contents(&config), "");
        config
            .set("providers.acme.key_secret", "ACME_API_KEY")
            .unwrap();
        assert!(contents(&config).contains("key_secret = \"ACME_API_KEY\""));
    }

    #[test]
    fn unparsable_file_is_not_overwritten_by_set() {
        let (_dir, config) = file(Some("[agent\n"));
        let err = config.set("display.theme", "mono").unwrap_err();
        assert_eq!(err.code().code, "config.parse");
        assert_eq!(contents(&config), "[agent\n");
    }

    #[test]
    fn unset_restores_the_default() {
        let (_dir, config) = file(Some("[agent]\nmax_iterations = 40\n"));
        assert!(config.unset("agent.max_iterations").unwrap());
        assert!(!config.unset("agent.max_iterations").unwrap());
        let effective = config.load(&crate::Overrides::default()).unwrap();
        assert_eq!(effective.config().agent.max_iterations, 500);
        assert_eq!(config.unset("agent.nope").unwrap_err().exit_code(), 2);
    }

    #[test]
    fn writes_keep_the_last_ten_versions() {
        let (_dir, config) = file(Some(""));
        for n in 0..12 {
            config.set("agent.max_iterations", &n.to_string()).unwrap();
        }
        let kept = write_backups(config.backups_dir());
        assert_eq!(kept.len(), KEEP);
        let newest = std::fs::read_to_string(kept.last().unwrap()).unwrap();
        assert!(newest.contains("max_iterations = 10"), "{newest}");
    }

    #[cfg(unix)]
    #[test]
    fn written_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, config) = file(None);
        config.set("display.theme", "mono").unwrap();
        let mode = std::fs::metadata(config.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn replace_validates_the_whole_document() {
        let (_dir, config) = file(Some("[agent\n"));
        let err = config.replace("[approvals]\nmode = \"of\"\n").unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert_eq!(contents(&config), "[agent\n");
        config.replace("[approvals]\nmode = \"smart\"\n").unwrap();
        assert_eq!(contents(&config), "[approvals]\nmode = \"smart\"\n");
        let err = config
            .replace("[display]\ntheme = \"ghp_abcdefghijklmnop\"\n")
            .unwrap_err();
        assert_eq!(err.code().code, "config.credential_in_file");
        assert_eq!(err.exit_code(), 78);
    }
}
