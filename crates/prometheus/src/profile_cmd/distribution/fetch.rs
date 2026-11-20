//! Getting a distribution's files: a directory is read in place, a git URL
//! is cloned (shallow) into a scratch directory under the home's cache.

use std::path::{Path, PathBuf};
use std::process::Command;

use pmt_core::error::{Error, codes};

/// A fetched distribution.
pub(super) struct Fetched {
    /// Where its files are.
    pub root: PathBuf,
    /// The commit it was read at, when the source is a git repository.
    pub commit: Option<String>,
    /// Holds a clone's scratch directory until dropped.
    _scratch: Option<tempfile::TempDir>,
}

/// Fetches `source` (a directory, else a git URL).
///
/// # Errors
///
/// `profile.distribution_fetch` (69) when the clone fails.
pub(super) fn fetch(home: &Path, source: &str) -> Result<Fetched, Error> {
    let dir = Path::new(source);
    if dir.is_dir() {
        return Ok(Fetched {
            root: dir.to_owned(),
            commit: head(dir),
            _scratch: None,
        });
    }
    let cache = home.join("cache").join("distributions");
    std::fs::create_dir_all(&cache).map_err(|e| Error::io(&codes::HOME_IO, &cache, &e))?;
    let scratch =
        tempfile::tempdir_in(&cache).map_err(|e| Error::io(&codes::HOME_IO, &cache, &e))?;
    let root = scratch.path().join("src");
    let out = Command::new("git")
        .args(["clone", "--quiet", "--depth", "1", "--", source])
        .arg(&root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| failed(source, &format!("cannot run git: {e}")))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(failed(source, &why));
    }
    Ok(Fetched {
        commit: head(&root),
        root,
        _scratch: Some(scratch),
    })
}

/// `HEAD` of the repository at `dir`, when it is one.
fn head(dir: &Path) -> Option<String> {
    if !dir.join(".git").exists() {
        return None;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|c| !c.is_empty())
}

fn failed(source: &str, why: &str) -> Error {
    Error::new(
        &codes::PROFILE_DISTRIBUTION_FETCH,
        format!("cannot fetch distribution {source}: {why}"),
    )
}
