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
    ACTIVE_PROFILE_FILE, DEFAULT_PROFILE, Home, ProfileDir, ProfileName, TOMBSTONE_DIR, io_error,
    private_dir,
};
use crate::scope::{Launch, ProfileScope};
use crate::secrets::SecretStore;

/// The profile's identity file, copied by `--clone-from`.
pub const IDENTITY_FILE: &str = "SOUL.md";
/// What `--clone-from` copies besides `config.toml` and the identity file.
pub const CLONED_DIRS: &[&str] = &["memories", "skills", "themes", "plugins"];

/// What a new profile starts from.
#[derive(Debug, Clone, Copy)]
pub struct CloneFrom<'a> {
    /// The source profile.
    pub source: &'a ProfileName,
    /// Also copy the source's secrets.
    pub secrets: bool,
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
        let entries = std::fs::read_dir(&dir).map_err(|e| io_error(&dir, &e))?;
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
            copy_profile(&source, &profile, clone.secrets)?;
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
            .map_err(|e| io_error(&path, &e))
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
        std::fs::rename(from.path(), &target).map_err(|e| io_error(&target, &e))?;
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
            .map_err(|e| io_error(&tombstone, &e))?;
        if store.kind() == crate::schema::SecretsBackend::Keychain {
            for secret in store.names()? {
                store.remove(&secret)?;
            }
        }
        std::fs::remove_dir_all(profile.path()).map_err(|e| io_error(profile.path(), &e))?;
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

/// One-time copy of config, identity, memory, skills, themes and plugins
/// (and secrets when asked) from `source` into the new `target`.
fn copy_profile(source: &ProfileDir, target: &ProfileDir, secrets: bool) -> Result<(), Error> {
    let source_store = profile_store(source)?;
    let copy = |from: &Path, to: &Path| -> Result<(), Error> {
        if from.is_dir() {
            private_fs::copy_tree(from, to).map_err(|e| io_error(to, &e))
        } else if from.is_file() {
            private_fs::copy_file(from, to).map_err(|e| io_error(to, &e))
        } else {
            Ok(())
        }
    };
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
    if secrets {
        let target_store = profile_store(target)?;
        for name in source_store.names()? {
            if let Some(value) = source_store.get(&name)? {
                target_store.set(&name, &value)?;
            }
        }
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
        Err(e) => Err(io_error(path, &e)),
    }
}

#[cfg(test)]
mod tests;
