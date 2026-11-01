//! Profile lifecycle: list, create (optionally cloned), use, rename and
//! delete (spec: configuration-and-profiles, Profile name rules, Profile
//! lifecycle commands, Safe profile deletion, Profile selection at process
//! start). Profiles are islands: nothing here makes one profile read
//! another at runtime; a clone is a one-time copy.

use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_core::runtime::{self, Role};

use crate::layers::Overrides;
use crate::layout::{
    ACTIVE_PROFILE_FILE, DEFAULT_PROFILE, Home, ProfileDir, ProfileName, TOMBSTONE_DIR, private_dir,
};
use crate::scope::{Launch, ProfileScope};
use crate::secrets::SecretStore;

/// The profile's identity file, copied by `--clone-from`.
pub const IDENTITY_FILE: &str = "SOUL.md";
/// What `--clone-from` copies besides `config.toml` and the identity file
/// (built-in memory lives in `state.db`; the CLI copies its entries).
pub const CLONED_DIRS: &[&str] = &["memories", "skills", "themes", "plugins"];

/// The free-text description `profile create --description` records.
pub const DESCRIPTION_FILE: &str = "description";

/// The import sync registry (spec: hermes-migration, Keeping imports in
/// sync), copied by a clone only with `--sync-imports`.
pub const IMPORT_SYNC_FILE: &str = "migrations/import-sync.toml";

/// Per-profile pairing data kept outside `state.db`, copied by a clone
/// only with `--clone-channels`.
pub const PAIRING_DIR: &str = "pairing";

/// What `--clone-all` never copies: the source's sessions and their
/// indexes, logs, snapshots, backups, caches, its identity and secrets
/// (secrets go through the store with `--clone-secrets`).
const NEVER_CLONED: &[&str] = &[
    "state.db",
    "state.db-wal",
    "state.db-shm",
    "blobs",
    "trajectories",
    "logs",
    "snapshots",
    "backups",
    "cache",
    "run",
    "profile_id",
    "secrets.toml",
    "secrets.toml.lock",
    DESCRIPTION_FILE,
];

/// What a new profile starts from (spec: configuration-and-profiles,
/// Profile lifecycle commands; Profile creation options).
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // independent copy options of one command line
pub struct CloneFrom<'a> {
    /// The source profile.
    pub source: &'a ProfileName,
    /// Also copy the source's secrets.
    pub secrets: bool,
    /// Copy everything except sessions, `state.db`, logs, snapshots and
    /// backups, instead of config, identity, memory, skills, themes and
    /// plugins only.
    pub all: bool,
    /// Keep the messaging adapter configuration (`gateway.platforms`) and
    /// pairing data, which a clone otherwise drops so two profiles never
    /// answer the same bot account.
    pub channels: bool,
    /// Also copy the import sync registry.
    pub sync_imports: bool,
}

impl<'a> CloneFrom<'a> {
    /// A `--clone-from` copy without secrets, channels or imports.
    #[must_use]
    pub fn of(source: &'a ProfileName) -> Self {
        Self {
            source,
            secrets: false,
            all: false,
            channels: false,
            sync_imports: false,
        }
    }
}

/// A running process that uses a profile, if any: `(role, pid)`.
///
/// # Errors
///
/// `home.io` when a runtime record cannot be read.
pub fn user_of(home: &Path, name: &ProfileName) -> Result<Option<(Role, u32)>, Error> {
    for role in [Role::Daemon, Role::Chat] {
        if let Some(record) = runtime::live_record(home, role)?
            && record.profiles.iter().any(|p| p == name.as_str())
        {
            return Ok(Some((role, record.pid)));
        }
    }
    Ok(None)
}

/// Fails with `profile.in_use` (exit 75) when a running process uses
/// `name`.
///
/// # Errors
///
/// `profile.in_use`, `home.io`.
pub fn ensure_unused(home: &Path, name: &ProfileName) -> Result<(), Error> {
    let Some((role, pid)) = user_of(home, name)? else {
        return Ok(());
    };
    let who = match role {
        Role::Daemon => "the gateway daemon",
        Role::Chat => "an interactive prometheus chat",
    };
    Err(Error::new(
        &codes::PROFILE_IN_USE,
        format!("profile '{name}' is in use by {who} (pid {pid})"),
    )
    .with_hint("stop it and retry"))
}

impl Home {
    /// Every profile, sorted (`default` included, deleted ones excluded).
    ///
    /// # Errors
    ///
    /// `home.io`.
    pub fn profiles(&self) -> Result<Vec<ProfileName>, Error> {
        let dir = self.root().join("profiles");
        let entries = std::fs::read_dir(&dir).map_err(|e| Error::io(&codes::HOME_IO, &dir, &e))?;
        let mut names: Vec<ProfileName> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|name| name != TOMBSTONE_DIR)
            .filter_map(|name| ProfileName::parse(&name).ok())
            .filter(|name| !self.tombstone(name).exists())
            .collect();
        names.sort();
        Ok(names)
    }

    /// Creates profile `name` (a reserved name is refused), copying from
    /// `clone` once when given. A tombstone of an earlier profile with this
    /// name is removed: creating is the explicit way to revive a name.
    ///
    /// # Errors
    ///
    /// `profile.invalid_name` / `profile.reserved` (2), `profile.exists`
    /// (2), `profile.not_found` for an unknown source, config errors of the
    /// source, `home.io`.
    pub fn create_profile(
        &self,
        name: &str,
        clone: Option<CloneFrom<'_>>,
    ) -> Result<ProfileDir, Error> {
        let name = ProfileName::parse_new(name)?;
        if self.profile_path(&name).exists() || name.as_str() == DEFAULT_PROFILE {
            return Err(Error::new(
                &codes::PROFILE_EXISTS,
                format!("profile '{name}' already exists"),
            ));
        }
        let source = clone.map(|c| self.existing_profile(c.source)).transpose()?;
        remove_file_if_exists(&self.tombstone(&name))?;
        let profile = self.profile(&name)?;
        if let (Some(source), Some(clone)) = (source, clone) {
            copy_profile(&source, &profile, clone)?;
        }
        Ok(profile)
    }

    /// Makes `name` the profile used without `-p`, writing
    /// `active_profile` atomically; `default` removes the file.
    ///
    /// # Errors
    ///
    /// `profile.not_found` (2), `home.io`.
    pub fn use_profile(&self, name: &ProfileName) -> Result<(), Error> {
        self.existing_profile(name)?;
        let path = self.root().join(ACTIVE_PROFILE_FILE);
        if name.as_str() == DEFAULT_PROFILE {
            return remove_file_if_exists(&path);
        }
        private_fs::write_atomic(&path, format!("{name}\n").as_bytes())
            .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))
    }

    /// Renames profile `old` to `new`, moving its keychain secrets and the
    /// `active_profile` selection with it.
    ///
    /// # Errors
    ///
    /// `profile.protected` for `default`, `profile.not_found`,
    /// `profile.exists`, name errors, `profile.in_use` (75), `home.io`.
    pub fn rename_profile(&self, old: &ProfileName, new: &str) -> Result<ProfileDir, Error> {
        protect_default(old, "renamed")?;
        let from = self.existing_profile(old)?;
        let new = ProfileName::parse_new(new)?;
        if self.profile_path(&new).exists() || new.as_str() == DEFAULT_PROFILE {
            return Err(Error::new(
                &codes::PROFILE_EXISTS,
                format!("profile '{new}' already exists"),
            ));
        }
        ensure_unused(self.root(), old)?;
        let old_store = profile_store(&from)?;
        remove_file_if_exists(&self.tombstone(&new))?;
        let target = self.profile_path(&new);
        std::fs::rename(from.path(), &target)
            .map_err(|e| Error::io(&codes::HOME_IO, &target, &e))?;
        let to = self.profile(&new)?;
        move_keychain_secrets(&old_store, &to)?;
        if self.active_profile()? == *old {
            self.use_profile(&new)?;
        }
        Ok(to)
    }

    /// Deletes profile `name`: refuses `default` and a profile in use,
    /// writes the tombstone first (so a concurrent process cannot recreate
    /// the directory), removes its keychain secrets and directory, and
    /// clears `active_profile` when it named the profile. Confirmation is
    /// the caller's.
    ///
    /// # Errors
    ///
    /// `profile.protected`, `profile.not_found`, `profile.in_use` (75),
    /// `home.io`.
    pub fn delete_profile(&self, name: &ProfileName) -> Result<(), Error> {
        protect_default(name, "deleted")?;
        let profile = self.existing_profile(name)?;
        ensure_unused(self.root(), name)?;
        let store = profile_store(&profile)?;
        let tombstone = self.tombstone(name);
        private_dir(&self.root().join("profiles").join(TOMBSTONE_DIR))?;
        let stamp = jiff::Timestamp::now().to_string();
        private_fs::write_atomic(&tombstone, format!("deleted {stamp}\n").as_bytes())
            .map_err(|e| Error::io(&codes::HOME_IO, &tombstone, &e))?;
        if store.kind() == crate::schema::SecretsBackend::Keychain {
            for secret in store.names()? {
                store.remove(&secret)?;
            }
        }
        std::fs::remove_dir_all(profile.path())
            .map_err(|e| Error::io(&codes::HOME_IO, profile.path(), &e))?;
        if self.active_profile().ok().as_ref() == Some(name) {
            remove_file_if_exists(&self.root().join(ACTIVE_PROFILE_FILE))?;
        }
        Ok(())
    }
}

fn protect_default(name: &ProfileName, verb: &str) -> Result<(), Error> {
    if name.as_str() == DEFAULT_PROFILE {
        return Err(Error::new(
            &codes::PROFILE_PROTECTED,
            format!("the default profile cannot be {verb}"),
        ));
    }
    Ok(())
}

/// The secrets store `profile`'s config selects.
fn profile_store(profile: &ProfileDir) -> Result<SecretStore, Error> {
    let scope = ProfileScope::open(profile.clone(), &Overrides::default(), Launch::Other)?;
    Ok(scope.secrets().store().clone())
}

/// One-time copy from `source` into the new `target`: config, identity,
/// skills, themes and plugins (or everything with `all`), then the
/// adjustments `clone` asks for.
fn copy_profile(
    source: &ProfileDir,
    target: &ProfileDir,
    clone: CloneFrom<'_>,
) -> Result<(), Error> {
    let source_store = profile_store(source)?;
    if clone.all {
        copy_all(source.path(), target.path())?;
    } else {
        copy(
            &source.path().join("config.toml"),
            &target.path().join("config.toml"),
        )?;
        copy(
            &source.path().join(IDENTITY_FILE),
            &target.path().join(IDENTITY_FILE),
        )?;
        for dir in CLONED_DIRS {
            copy(&source.path().join(dir), &target.path().join(dir))?;
        }
    }
    let imports = target.path().join(IMPORT_SYNC_FILE);
    if clone.sync_imports {
        copy(&source.path().join(IMPORT_SYNC_FILE), &imports)?;
    } else {
        remove_file_if_exists(&imports)?;
    }
    if !clone.channels {
        drop_channels(target)?;
    }
    if clone.secrets {
        let target_store = profile_store(target)?;
        for name in source_store.names()? {
            if let Some(value) = source_store.get(&name)? {
                target_store.set(&name, &value)?;
            }
        }
    }
    Ok(())
}

fn copy(from: &Path, to: &Path) -> Result<(), Error> {
    if let Some(parent) = to.parent() {
        private_dir(parent)?;
    }
    if from.is_dir() {
        private_fs::copy_tree(from, to).map_err(|e| Error::io(&codes::HOME_IO, to, &e))
    } else if from.is_file() {
        private_fs::copy_file(from, to).map_err(|e| Error::io(&codes::HOME_IO, to, &e))
    } else {
        Ok(())
    }
}

/// Every top-level entry of `from` except [`NEVER_CLONED`].
fn copy_all(from: &Path, to: &Path) -> Result<(), Error> {
    let entries = std::fs::read_dir(from).map_err(|e| Error::io(&codes::HOME_IO, from, &e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(&codes::HOME_IO, from, &e))?;
        let name = entry.file_name();
        if NEVER_CLONED.iter().any(|n| name == *n) {
            continue;
        }
        copy(&entry.path(), &to.join(&name))?;
    }
    Ok(())
}

/// Removes the clone's messaging adapters and pairing data.
fn drop_channels(target: &ProfileDir) -> Result<(), Error> {
    let pairing = target.path().join(PAIRING_DIR);
    if pairing.exists() {
        std::fs::remove_dir_all(&pairing).map_err(|e| Error::io(&codes::HOME_IO, &pairing, &e))?;
    }
    let file = target.config_file();
    let Some(text) = file.raw()? else {
        return Ok(());
    };
    let mut doc: toml_edit::DocumentMut = match text.parse() {
        Ok(doc) => doc,
        Err(_) => return Ok(()),
    };
    if crate::edit::remove(&mut doc, "gateway.platforms") {
        private_fs::write_atomic(file.path(), doc.to_string().as_bytes())
            .map_err(|e| Error::io(&codes::HOME_IO, file.path(), &e))?;
    }
    Ok(())
}

/// Keychain entries are keyed by profile name, so a rename moves them; a
/// file store moved with the directory.
fn move_keychain_secrets(old: &SecretStore, renamed: &ProfileDir) -> Result<(), Error> {
    if old.kind() != crate::schema::SecretsBackend::Keychain {
        return Ok(());
    }
    let new = profile_store(renamed)?;
    for name in old.names()? {
        if let Some(value) = old.get(&name)? {
            new.set(&name, &value)?;
            old.remove(&name)?;
        }
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<(), Error> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(&codes::HOME_IO, path, &e)),
    }
}

#[cfg(test)]
mod tests;
