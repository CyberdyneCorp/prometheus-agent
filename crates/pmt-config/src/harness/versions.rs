//! `<profile>/harness/`: immutable versions and the `ACTIVE` pointer.
//!
//! A version is written once, under a temporary name renamed into place,
//! so a reader never sees a half-written tree; nothing here deletes one.
//! Writers serialize on `ACTIVE.lock`, so two writers never take the same
//! sequence number.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

use super::{Census, Tree};

/// Where a version came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The built-in harness of a fresh profile.
    Builtin,
    /// A user edit (`harness apply`, `harness edit`).
    Manual,
    /// An adopted evolution proposal.
    Evolve,
    /// `harness rollback`.
    Rollback,
    /// Imported from elsewhere (an experience library entry).
    Import,
}

impl Source {
    /// The spelling in `version.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Manual => "manual",
            Self::Evolve => "evolve",
            Self::Rollback => "rollback",
            Self::Import => "import",
        }
    }
}

/// A version's `version.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// `h<sequence>-<fingerprint>`.
    pub id: String,
    /// The version that was active when this one was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// [`Tree::fingerprint`].
    pub fingerprint: String,
    /// Where it came from.
    pub source: Source,
    /// The evolution job that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// What the tree holds.
    pub census: Census,
}

impl VersionInfo {
    /// The sequence number of the id.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        sequence_of(&self.id).unwrap_or(0)
    }
}

fn sequence_of(id: &str) -> Option<u64> {
    let rest = id.strip_prefix('h')?;
    let digits = rest.split('-').next()?;
    digits.parse().ok()
}

/// Whether `id` has the shape of a version id, `h<n>-<fingerprint>`.
/// References come from clients (RPC, CLI) and are joined into paths, so
/// anything else (`../`, separators) is not found before it reaches the
/// filesystem.
fn is_version_id(id: &str) -> bool {
    sequence_of(id).is_some()
        && id.split_once('-').is_some_and(|(_, rest)| {
            !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric())
        })
}

/// One profile's harness directory.
#[derive(Debug, Clone)]
pub struct HarnessDir {
    root: PathBuf,
}

const ACTIVE: &str = "ACTIVE";
const VERSIONS: &str = "versions";
const VERSION_FILE: &str = "version.toml";

fn io_error(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::HARNESS_IO, path, err)
}

impl HarnessDir {
    /// The harness of the profile at `profile_dir`.
    #[must_use]
    pub fn of(profile_dir: &Path) -> Self {
        Self {
            root: super::dir(profile_dir),
        }
    }

    /// `<profile>/harness`.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `tree/` directory of version `id`.
    #[must_use]
    pub fn tree_dir(&self, id: &str) -> PathBuf {
        self.root.join(VERSIONS).join(id).join("tree")
    }

    /// Writes the built-in version `h1` and activates it when the profile
    /// has no `ACTIVE` yet; `None` when it already has one.
    ///
    /// # Errors
    ///
    /// `harness.io`.
    pub fn seed(&self) -> Result<Option<VersionInfo>, Error> {
        if self.root.join(ACTIVE).exists() {
            return Ok(None);
        }
        let _lock = self.lock()?;
        if self.root.join(ACTIVE).exists() {
            return Ok(None);
        }
        let info = self.write_locked(&Tree::builtin(), Source::Builtin, None, None)?;
        self.activate_locked(&info.id)?;
        Ok(Some(info))
    }

    /// The active version id, seeding `h1` first when there is none.
    ///
    /// # Errors
    ///
    /// `harness.io`.
    pub fn active_id(&self) -> Result<String, Error> {
        self.seed()?;
        let path = self.root.join(ACTIVE);
        private_fs::read_no_follow(&path)
            .map(|text| text.trim().to_owned())
            .map_err(|e| io_error(&path, &e))
    }

    /// The active version.
    ///
    /// # Errors
    ///
    /// `harness.io`; `harness.version_not_found` when `ACTIVE` names a
    /// missing version.
    pub fn active(&self) -> Result<VersionInfo, Error> {
        self.version(&self.active_id()?)
    }

    /// Every version, newest first.
    ///
    /// # Errors
    ///
    /// `harness.io` (also for a `version.toml` that does not parse).
    pub fn versions(&self) -> Result<Vec<VersionInfo>, Error> {
        self.seed()?;
        let dir = self.root.join(VERSIONS);
        let entries = std::fs::read_dir(&dir).map_err(|e| io_error(&dir, &e))?;
        let mut versions = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_error(&dir, &e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if sequence_of(&name).is_some() {
                versions.push(self.version(&name)?);
            }
        }
        versions.sort_by_key(|v| std::cmp::Reverse(v.sequence()));
        Ok(versions)
    }

    /// Version `id`.
    ///
    /// # Errors
    ///
    /// `harness.version_not_found`; `harness.io`.
    pub fn version(&self, id: &str) -> Result<VersionInfo, Error> {
        if !is_version_id(id) {
            return Err(not_found(id));
        }
        let path = self.root.join(VERSIONS).join(id).join(VERSION_FILE);
        let text = match private_fs::read_no_follow(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(not_found(id));
            }
            Err(e) => return Err(io_error(&path, &e)),
        };
        toml::from_str(&text).map_err(|e| {
            Error::new(
                &codes::HARNESS_IO,
                format!("{}: {}", path.display(), e.message()),
            )
        })
    }

    /// The version a reference names: a full id, or `h<n>`.
    ///
    /// # Errors
    ///
    /// `harness.version_not_found`; `harness.io`.
    pub fn resolve(&self, reference: &str) -> Result<VersionInfo, Error> {
        if reference.contains('-') {
            return self.version(reference);
        }
        let wanted = sequence_of(reference).ok_or_else(|| not_found(reference))?;
        self.versions()?
            .into_iter()
            .find(|v| v.sequence() == wanted)
            .ok_or_else(|| not_found(reference))
    }

    /// The tree of version `id`.
    ///
    /// # Errors
    ///
    /// `harness.version_not_found` for a malformed id; `harness.io`, also
    /// for a tree holding a symlink or non-UTF-8 file.
    pub fn tree(&self, id: &str) -> Result<Tree, Error> {
        if !is_version_id(id) {
            return Err(not_found(id));
        }
        let dir = self.tree_dir(id);
        let (tree, problems) = Tree::read(&dir).map_err(|e| io_error(&dir, &e))?;
        match problems.first() {
            Some(problem) => Err(Error::new(
                &codes::HARNESS_IO,
                format!("harness version {id}: {problem}"),
            )),
            None => Ok(tree),
        }
    }

    /// Writes `tree` as a new version (not activated); its parent is the
    /// active version.
    ///
    /// # Errors
    ///
    /// `harness.io`.
    pub fn write(
        &self,
        tree: &Tree,
        source: Source,
        job_id: Option<&str>,
    ) -> Result<VersionInfo, Error> {
        let parent = self.active_id()?;
        let _lock = self.lock()?;
        self.write_locked(tree, source, job_id, Some(parent))
    }

    /// Makes version `id` the one new sessions load.
    ///
    /// # Errors
    ///
    /// `harness.version_not_found`; `harness.io`.
    pub fn activate(&self, id: &str) -> Result<(), Error> {
        self.version(id)?;
        let _lock = self.lock()?;
        self.activate_locked(id)
    }

    fn lock(&self) -> Result<std::fs::File, Error> {
        private_fs::create_dir(&self.root).map_err(|e| io_error(&self.root, &e))?;
        let path = self.root.join(ACTIVE);
        private_fs::lock_beside(&path).map_err(|e| io_error(&path, &e))
    }

    fn activate_locked(&self, id: &str) -> Result<(), Error> {
        let path = self.root.join(ACTIVE);
        private_fs::write_atomic(&path, format!("{id}\n").as_bytes())
            .map_err(|e| io_error(&path, &e))
    }

    fn write_locked(
        &self,
        tree: &Tree,
        source: Source,
        job_id: Option<&str>,
        parent: Option<String>,
    ) -> Result<VersionInfo, Error> {
        let versions = self.root.join(VERSIONS);
        private_fs::create_dir(&versions).map_err(|e| io_error(&versions, &e))?;
        let next = std::fs::read_dir(&versions)
            .map_err(|e| io_error(&versions, &e))?
            .filter_map(Result::ok)
            .filter_map(|e| sequence_of(&e.file_name().to_string_lossy()))
            .max()
            .unwrap_or(0)
            + 1;
        let fingerprint = tree.fingerprint();
        let info = VersionInfo {
            id: format!("h{next}-{fingerprint}"),
            parent,
            fingerprint,
            source,
            job_id: job_id.map(str::to_owned),
            created_at: jiff::Timestamp::now().to_string(),
            census: Census::of(tree),
        };
        let staging = versions.join(format!(".tmp-{}-{}", info.id, std::process::id()));
        let written = (|| {
            tree.write(&staging.join("tree"))?;
            let text = toml::to_string(&info).map_err(std::io::Error::other)?;
            std::fs::write(staging.join(VERSION_FILE), text)?;
            std::fs::rename(&staging, versions.join(&info.id))
        })();
        if let Err(err) = written {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(io_error(&versions, &err));
        }
        Ok(info)
    }
}

fn not_found(reference: &str) -> Error {
    Error::new(
        &codes::HARNESS_VERSION_NOT_FOUND,
        format!("no harness version '{reference}'"),
    )
    .with_hint("run: prometheus harness log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_profile_gets_the_builtin_version_once() {
        let dir = tempfile::tempdir().unwrap();
        let harness = HarnessDir::of(dir.path());
        let seeded = harness.seed().unwrap().expect("seeded");
        assert_eq!(seeded.id, format!("h1-{}", Tree::builtin().fingerprint()));
        assert_eq!(seeded.source, Source::Builtin);
        assert!(harness.seed().unwrap().is_none());
        assert_eq!(harness.active_id().unwrap(), seeded.id);
        assert_eq!(harness.tree(&seeded.id).unwrap(), Tree::builtin());
    }

    #[test]
    fn written_versions_take_the_next_sequence_and_resolve_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let harness = HarnessDir::of(dir.path());
        let mut tree = Tree::builtin();
        tree.files.insert("SYSTEM.md".into(), "be brief".into());
        let h2 = harness.write(&tree, Source::Manual, None).unwrap();
        assert!(h2.id.starts_with("h2-"), "{}", h2.id);
        assert_eq!(
            h2.parent.as_deref(),
            Some(harness.active_id().unwrap().as_str())
        );
        assert_eq!(harness.resolve("h2").unwrap(), h2);
        assert_eq!(harness.versions().unwrap()[0], h2);
        assert_eq!(
            harness.resolve("h9").unwrap_err().code().code,
            "harness.version_not_found"
        );
    }

    /// A reference that would leave the versions directory is not found.
    #[test]
    fn references_that_leave_the_versions_directory_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let harness = HarnessDir::of(dir.path());
        let active = harness.active_id().unwrap();
        for reference in [
            "../../../other/harness/versions/h1-abc".to_owned(),
            format!("h1-x/../../{active}"),
            "h1-".to_owned(),
        ] {
            for err in [
                harness.resolve(&reference).unwrap_err(),
                harness.tree(&reference).unwrap_err(),
            ] {
                assert_eq!(err.code().code, "harness.version_not_found", "{reference}");
            }
        }
    }
}
