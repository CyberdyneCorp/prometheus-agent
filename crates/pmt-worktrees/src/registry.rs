//! The repositories a profile made trees in (spec: git-worktrees,
//! Repository registry and sweep schedule): `<profile>/worktree_repos.json`,
//! read by the sweep at `chat -w` start and every 6 hours.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::repo::Repo;
use crate::sweep::{self, Decision, Mode, Settings};

/// The registry file under the profile directory.
pub const FILE: &str = "worktree_repos.json";

/// How often the scheduler sweeps every recorded repository.
pub const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_hours(6);

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    repos: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    added: u64,
}

fn read(profile: &Path) -> File {
    std::fs::read(profile.join(FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write(profile: &Path, file: &File) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(file).map_err(std::io::Error::other)?;
    pmt_core::private_fs::write_atomic(&profile.join(FILE), &bytes)
}

/// Records `repo` for profile directory `profile` (once).
///
/// # Errors
///
/// The registry cannot be written.
pub fn record(profile: &Path, repo: &Repo) -> std::io::Result<()> {
    let _guard = pmt_core::private_fs::lock_beside(&profile.join(FILE))?;
    let mut file = read(profile);
    if file.repos.iter().any(|e| e.path == repo.root) {
        return Ok(());
    }
    file.repos.push(Entry {
        path: repo.root.clone(),
        added: crate::tree::now_secs(),
    });
    write(profile, &file)
}

/// The recorded repositories that still exist; entries whose repository
/// is gone are dropped from the file.
#[must_use]
pub fn repos(profile: &Path) -> Vec<Repo> {
    let Ok(_guard) = pmt_core::private_fs::lock_beside(&profile.join(FILE)) else {
        return Vec::new();
    };
    let mut file = read(profile);
    let before = file.repos.len();
    let mut found = Vec::new();
    file.repos
        .retain(|entry| match Repo::discover(&entry.path) {
            Ok(repo)
                if repo.root == entry.path
                    || repo.root == entry.path.canonicalize().unwrap_or_default() =>
            {
                found.push(repo);
                true
            }
            _ => false,
        });
    if file.repos.len() != before
        && let Err(err) = write(profile, &file)
    {
        tracing::warn!(error = %err, "worktree registry not rewritten");
    }
    found
}

/// One repository's sweep result.
#[derive(Debug)]
pub struct Swept {
    /// The repository.
    pub repo: Repo,
    /// Its decisions, or why it could not be swept.
    pub decisions: Result<Vec<Decision>, pmt_core::error::Error>,
}

/// Sweeps every recorded repository of `profile` at `now`.
#[must_use]
pub fn sweep_all(profile: &Path, settings: &Settings, now: u64, mode: Mode) -> Vec<Swept> {
    repos(profile)
        .into_iter()
        .map(|repo| {
            let decisions = sweep::sweep(&repo, settings, now, mode);
            Swept { repo, decisions }
        })
        .collect()
}
