//! The Hermes home, read-only (spec: hermes-migration, Source home is
//! never modified; Profile discovery and mapping).
//!
//! Every read here opens files for reading only; nothing under the Hermes
//! home is created, locked, chmod-ed or opened with SQLite. `state.db` is
//! read through [`snapshot`], a copy in a staging directory of the
//! destination.

use std::path::{Path, PathBuf};

use pmt_config::ProfileName;
use pmt_config::layout::RESERVED_NAMES;
use pmt_core::error::{Error, codes};

use crate::report::SkippedProfile;

/// The Hermes root profile's name.
pub const ROOT_PROFILE: &str = "default";

/// Files whose presence makes a directory a Hermes profile.
const MARKERS: &[&str] = &["config.yaml", ".env", "SOUL.md", "state.db"];

/// A Hermes home.
#[derive(Debug, Clone)]
pub struct HermesHome {
    root: PathBuf,
}

/// One Hermes profile and where it goes.
#[derive(Debug, Clone)]
pub struct SourceProfile {
    /// The Hermes name (`default` for the root).
    pub name: String,
    /// Its directory.
    pub dir: PathBuf,
}

impl SourceProfile {
    /// A file of the profile.
    #[must_use]
    pub fn path(&self, relative: &str) -> PathBuf {
        self.dir.join(relative)
    }

    /// The text of `relative`, `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// `migrate.component` when it exists but cannot be read.
    pub fn read(&self, relative: &str) -> Result<Option<String>, Error> {
        read_text(&self.path(relative))
    }
}

/// The text of `path`, `None` when it does not exist.
///
/// # Errors
///
/// `migrate.component` when it exists but cannot be read.
pub fn read_text(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(&codes::MIGRATE_COMPONENT, path, &e)),
    }
}

impl HermesHome {
    /// The Hermes home at `root`.
    ///
    /// # Errors
    ///
    /// `migrate.source_missing` (66) when `root` is not a directory.
    pub fn open(root: &Path) -> Result<Self, Error> {
        if !root.is_dir() {
            return Err(Error::new(
                &codes::MIGRATE_SOURCE_MISSING,
                format!("No Hermes home found at {}", root.display()),
            ));
        }
        Ok(Self {
            root: root.to_owned(),
        })
    }

    /// The home directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The sticky active profile (`active_profile`, `default` when unset).
    #[must_use]
    pub fn active_profile(&self) -> String {
        read_text(&self.root.join("active_profile"))
            .ok()
            .flatten()
            .map(|t| t.trim_start_matches('\u{feff}').trim().to_owned())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| ROOT_PROFILE.to_owned())
    }

    /// Every live profile (the root first) and the directories left out.
    #[must_use]
    pub fn profiles(&self) -> (Vec<SourceProfile>, Vec<SkippedProfile>) {
        let mut live = vec![SourceProfile {
            name: ROOT_PROFILE.to_owned(),
            dir: self.root.clone(),
        }];
        let mut skipped = Vec::new();
        let dir = self.root.join("profiles");
        let tombstones = dir.join(".deleted");
        let mut names: Vec<String> = list_dir(&dir)
            .into_iter()
            .chain(list_dir(&tombstones))
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        names.dedup();
        for name in names {
            let path = dir.join(&name);
            if tombstones.join(&name).exists() {
                skipped.push(skip(&name, "tombstoned"));
            } else if !path.is_dir() {
                // A plain file under profiles/ is not a profile.
            } else if MARKERS.iter().any(|m| path.join(m).exists()) {
                live.push(SourceProfile { name, dir: path });
            } else {
                skipped.push(skip(&name, "no Hermes profile files"));
            }
        }
        (live, skipped)
    }
}

fn skip(name: &str, reason: &str) -> SkippedProfile {
    SkippedProfile {
        name: name.to_owned(),
        reason: reason.to_owned(),
    }
}

fn list_dir(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The Prometheus profile a Hermes profile goes to: `into` when given,
/// else the same name, which must be valid and not reserved.
///
/// # Errors
///
/// `profile.reserved` naming `--into-profile`, `profile.invalid_name`.
pub fn target_name(hermes: &str, into: Option<&str>) -> Result<ProfileName, Error> {
    if let Some(into) = into {
        return if into == ROOT_PROFILE {
            Ok(ProfileName::default_profile())
        } else {
            ProfileName::parse_new(into)
        };
    }
    if hermes == ROOT_PROFILE {
        return Ok(ProfileName::default_profile());
    }
    let lower = hermes.to_lowercase();
    if RESERVED_NAMES.contains(&lower.as_str()) {
        return Err(Error::new(
            &codes::PROFILE_RESERVED,
            format!("profile name '{hermes}' is reserved in Prometheus; use --into-profile"),
        ));
    }
    ProfileName::parse(hermes).map_err(|_| {
        Error::new(
            &codes::PROFILE_INVALID_NAME,
            format!("profile name '{hermes}' is not valid in Prometheus; use --into-profile"),
        )
    })
}

/// Whether a Hermes gateway of the profile is running (`gateway.pid`
/// names a live process). Read-only: the pid file is only read.
#[must_use]
pub fn hermes_running(profile: &SourceProfile) -> bool {
    let Ok(Some(text)) = read_text(&profile.path("gateway.pid")) else {
        return false;
    };
    let pid = serde_json::from_str::<serde_json::Value>(text.trim())
        .ok()
        .and_then(|v| match v {
            serde_json::Value::Number(n) => n.as_u64(),
            serde_json::Value::Object(o) => o.get("pid").and_then(serde_json::Value::as_u64),
            _ => None,
        });
    pid.and_then(|p| u32::try_from(p).ok())
        .is_some_and(|p| pmt_core::runtime::process_start_time(p).is_some())
}

/// A consistent copy of a Hermes `state.db` (with its `-wal` and `-shm`)
/// in a staging directory; the directory is removed on drop.
#[derive(Debug)]
pub struct Snapshot {
    dir: tempfile::TempDir,
}

impl Snapshot {
    /// The copied database.
    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.dir.path().join("state.db")
    }
}

/// How often a copy is retried while Hermes writes to the source.
const COPY_ATTEMPTS: usize = 5;

/// Copies `profile`'s `state.db` into a fresh directory under `staging`
/// (created when missing). A copy taken while Hermes wrote the source (its
/// files' sizes or times changed during the copy) is retaken, so the copy
/// is one consistent state of the database.
///
/// # Errors
///
/// `migrate.component` when the files cannot be copied, `home.io` for
/// the staging directory.
pub fn snapshot(profile: &SourceProfile, staging: &Path) -> Result<Option<Snapshot>, Error> {
    let source = profile.path("state.db");
    if !source.is_file() {
        return Ok(None);
    }
    std::fs::create_dir_all(staging)
        .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot create", staging, &e))?;
    let dir = tempfile::Builder::new()
        .prefix("migrate-")
        .tempdir_in(staging)
        .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot create", staging, &e))?;
    let names = ["state.db", "state.db-wal", "state.db-shm"];
    for _ in 0..COPY_ATTEMPTS {
        let before = stamps(&profile.dir, &names);
        for name in names {
            copy_if_present(&profile.dir.join(name), &dir.path().join(name))?;
        }
        if stamps(&profile.dir, &names) == before {
            return Ok(Some(Snapshot { dir }));
        }
    }
    Ok(Some(Snapshot { dir }))
}

type Stamp = Option<(u64, Option<std::time::SystemTime>)>;

fn stamps(dir: &Path, names: &[&str]) -> Vec<Stamp> {
    names
        .iter()
        .map(|n| {
            std::fs::metadata(dir.join(n))
                .ok()
                .map(|m| (m.len(), m.modified().ok()))
        })
        .collect()
}

fn copy_if_present(from: &Path, to: &Path) -> Result<(), Error> {
    match std::fs::read(from) {
        Ok(bytes) => std::fs::write(to, bytes)
            .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot write", to, &e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_file(to);
            Ok(())
        }
        Err(e) => Err(Error::io(&codes::MIGRATE_COMPONENT, from, &e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_and_invalid_names_need_a_target() {
        let err = target_name("tools", None).unwrap_err();
        assert_eq!(
            err.message(),
            "profile name 'tools' is reserved in Prometheus; use --into-profile"
        );
        assert!(target_name("Bad Name", None).is_err());
        assert_eq!(
            target_name("tools", Some("toolbox")).unwrap().as_str(),
            "toolbox"
        );
        assert_eq!(target_name("default", None).unwrap().as_str(), "default");
    }
}
