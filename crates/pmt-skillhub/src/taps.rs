//! Custom skill taps (spec: skills, Custom skill taps): GitHub
//! repositories whose skill directories live under one path, kept in
//! `<profile>/skills/taps.toml`.
//!
//! ```toml
//! [[tap]]
//! repo = "my-org/agent-skills"
//! path = "skills/"
//! ```

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_skills::roots::profile_skills;
use serde::{Deserialize, Serialize};

/// The taps file name.
pub const FILE: &str = "taps.toml";

/// The default skills path of a tap.
pub const DEFAULT_PATH: &str = "skills/";

/// One tap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tap {
    /// `owner/repo`.
    pub repo: String,
    /// Where its skill directories live.
    #[serde(default = "default_path")]
    pub path: String,
}

fn default_path() -> String {
    DEFAULT_PATH.to_owned()
}

impl Tap {
    /// The repository path of skill `name` in this tap.
    #[must_use]
    pub fn skill_path(&self, name: &str) -> String {
        let base = self.path.trim_matches('/');
        if base.is_empty() {
            name.to_owned()
        } else {
            format!("{base}/{name}")
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "tap")]
    taps: Vec<Tap>,
}

fn path(profile_dir: &Path) -> PathBuf {
    profile_skills(profile_dir).join(FILE)
}

fn invalid(why: impl Into<String>) -> Error {
    Error::new(&codes::SKILL_INVALID, why)
}

/// The profile's taps.
///
/// # Errors
///
/// `skill.invalid` when the file does not parse, `skill.io`.
pub fn read(profile_dir: &Path) -> Result<Vec<Tap>, Error> {
    let file = path(profile_dir);
    match std::fs::read_to_string(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(Error::io(&codes::SKILL_IO, &file, &e)),
        Ok(text) => toml::from_str::<File>(&text)
            .map(|f| f.taps)
            .map_err(|e| invalid(format!("{}: {e}", file.display()))),
    }
}

fn write(profile_dir: &Path, taps: Vec<Tap>) -> Result<(), Error> {
    let file = path(profile_dir);
    let text = toml::to_string(&File { taps }).map_err(|e| invalid(e.to_string()))?;
    if let Some(dir) = file.parent() {
        private_fs::create_dir(dir).map_err(|e| Error::io(&codes::SKILL_IO, dir, &e))?;
    }
    private_fs::write_atomic(&file, text.as_bytes())
        .map_err(|e| Error::io(&codes::SKILL_IO, &file, &e))
}

/// Whether `repo` is `owner/repo`.
#[must_use]
pub fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// Adds (or re-points) tap `repo`; `true` when it is new.
///
/// # Errors
///
/// `skill.invalid` for a malformed repo or path, `skill.io`.
pub fn add(profile_dir: &Path, repo: &str, skills_path: Option<&str>) -> Result<bool, Error> {
    if !valid_repo(repo) {
        return Err(invalid(format!("'{repo}' is not owner/repo")));
    }
    let skills_path = skills_path.unwrap_or(DEFAULT_PATH);
    if skills_path.split('/').any(|p| p == "..") || skills_path.starts_with('/') {
        return Err(invalid(format!(
            "tap path '{skills_path}' must stay inside the repository"
        )));
    }
    let mut taps = read(profile_dir)?;
    let new = !taps.iter().any(|t| t.repo == repo);
    taps.retain(|t| t.repo != repo);
    taps.push(Tap {
        repo: repo.to_owned(),
        path: skills_path.to_owned(),
    });
    taps.sort_by(|a, b| a.repo.cmp(&b.repo));
    write(profile_dir, taps)?;
    Ok(new)
}

/// Removes tap `repo`.
///
/// # Errors
///
/// `skill.not_found` when there is no such tap, `skill.io`.
pub fn remove(profile_dir: &Path, repo: &str) -> Result<(), Error> {
    let mut taps = read(profile_dir)?;
    let before = taps.len();
    taps.retain(|t| t.repo != repo);
    if taps.len() == before {
        return Err(Error::new(
            &codes::SKILL_NOT_FOUND,
            format!("no tap '{repo}'"),
        ));
    }
    write(profile_dir, taps)
}
