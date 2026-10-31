//! `<profile>/skills/skills.lock`: where each non-user skill of the profile
//! came from (spec: skills, Bundled skills are read-only and ejectable;
//! Load-time template substitution and inline shell: trust level).
//!
//! M1 records ejected bundled skills (`source = "bundled:<path>"`, the
//! bundled tree's digest at eject time) and reads `trust` for the inline
//! shell rule; hub installs (M2) add their entries with the same fields.
//! A file that does not parse makes every profile skill's provenance
//! unreadable, which only ever withholds privileges.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

/// The lockfile name in the profile skills directory.
pub const FILE: &str = "skills.lock";
/// The `source` prefix of an ejected bundled skill.
pub const BUNDLED_SOURCE: &str = "bundled:";

/// How far a skill's origin is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// Shipped with Prometheus (bundled, official).
    Builtin,
    /// From a source in `skills.trusted_sources`.
    Trusted,
    /// From any other hub source.
    Community,
}

/// One entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The skill name.
    pub name: String,
    /// `bundled:<path>` or a hub identifier.
    pub source: String,
    /// The origin's digest when recorded.
    pub content_digest: String,
    /// The trust level (`None` when unreadable).
    #[serde(default, deserialize_with = "lenient_trust")]
    pub trust: Option<Trust>,
    /// RFC 3339 time of the install or eject.
    pub installed_at: String,
}

fn lenient_trust<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Trust>, D::Error> {
    let value = Option::<toml::Value>::deserialize(d)?;
    Ok(value.and_then(|v| v.try_into().ok()))
}

impl Entry {
    /// The bundled path of an ejected skill.
    #[must_use]
    pub fn bundled_path(&self) -> Option<&str> {
        self.source.strip_prefix(BUNDLED_SOURCE)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "skill")]
    skills: Vec<Entry>,
}

/// The lockfile of a profile skills directory.
#[derive(Debug, Clone)]
pub struct LockFile {
    path: PathBuf,
}

/// What a lockfile read gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Contents {
    /// The entries (none when the file is missing).
    Entries(Vec<Entry>),
    /// The file exists but does not parse.
    Unreadable(String),
}

impl Contents {
    /// The entry of `name`; `Err` when provenance is unreadable.
    ///
    /// # Errors
    ///
    /// The parse failure.
    pub fn entry(&self, name: &str) -> Result<Option<&Entry>, &str> {
        match self {
            Self::Entries(entries) => Ok(entries.iter().find(|e| e.name == name)),
            Self::Unreadable(why) => Err(why),
        }
    }
}

impl LockFile {
    /// The lockfile in `skills_dir`.
    #[must_use]
    pub fn in_dir(skills_dir: &Path) -> Self {
        Self {
            path: skills_dir.join(FILE),
        }
    }

    /// Its path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the entries.
    #[must_use]
    pub fn read(&self) -> Contents {
        match std::fs::read_to_string(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Contents::Entries(Vec::new()),
            Err(e) => Contents::Unreadable(format!("{}: {e}", self.path.display())),
            Ok(text) => match toml::from_str::<File>(&text) {
                Ok(file) => Contents::Entries(file.skills),
                Err(e) => Contents::Unreadable(format!("{}: {e}", self.path.display())),
            },
        }
    }

    /// Replaces the entry of `entry.name` (or adds it).
    ///
    /// # Errors
    ///
    /// `skill.io` when the file is unreadable or cannot be written.
    pub fn upsert(&self, entry: Entry) -> Result<(), Error> {
        self.update(|entries| {
            entries.retain(|e| e.name != entry.name);
            entries.push(entry);
        })
    }

    /// Removes the entry of `name`.
    ///
    /// # Errors
    ///
    /// `skill.io`.
    pub fn remove(&self, name: &str) -> Result<(), Error> {
        self.update(|entries| entries.retain(|e| e.name != name))
    }

    fn update(&self, change: impl FnOnce(&mut Vec<Entry>)) -> Result<(), Error> {
        let mut entries = match self.read() {
            Contents::Entries(entries) => entries,
            Contents::Unreadable(why) => return Err(Error::new(&codes::SKILL_IO, why)),
        };
        change(&mut entries);
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let text = toml::to_string(&File { skills: entries })
            .map_err(|e| Error::new(&codes::SKILL_IO, format!("skills.lock: {e}")))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| io(dir, &e))?;
        }
        private_fs::write_atomic(&self.path, text.as_bytes()).map_err(|e| io(&self.path, &e))
    }
}

/// An RFC 3339 now.
#[must_use]
pub fn now() -> String {
    Timestamp::now().to_string()
}

pub(crate) fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::new(&codes::SKILL_IO, format!("{}: {e}", path.display()))
}
