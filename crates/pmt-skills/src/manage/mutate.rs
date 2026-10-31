//! One skill write: under the skill's exclusive lock, atomic per file,
//! then a security scan of the resulting skill that restores the previous
//! state on a `dangerous` verdict or a scanner error (spec: skills, Skill
//! mutation safety and fail-closed scanning).

use std::fs;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use pmt_core::private_fs;

use crate::layout;
use crate::scan::{Report, Scanner, Verdict};

/// One file change, relative to the skill directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Write (create or replace) a file.
    Write(String, String),
    /// Remove a file.
    Remove(String),
}

impl Change {
    fn rel(&self) -> &str {
        match self {
            Self::Write(rel, _) | Self::Remove(rel) => rel,
        }
    }
}

/// `<skills>/.locks/<name>.lock`, held for the whole mutation.
///
/// # Errors
///
/// The lock cannot be taken.
pub fn lock(skills_dir: &Path, name: &str) -> Result<fs::File, String> {
    let dir = skills_dir.join(".locks");
    private_fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    private_fs::lock_beside(&dir.join(name)).map_err(|e| format!("skill lock: {e}"))
}

/// Applies `changes` to `dir`, scans the result and keeps it only when the
/// scan passes; otherwise every file is put back (a new directory is
/// removed) and the refusal is returned.
///
/// # Errors
///
/// The refusal or I/O failure text for the model.
pub fn commit(dir: &Path, changes: &[Change], scanner: &dyn Scanner) -> Result<Report, String> {
    let existed = dir.exists();
    let before: Vec<(String, Option<Vec<u8>>)> = changes
        .iter()
        .map(|c| (c.rel().to_owned(), fs::read(dir.join(c.rel())).ok()))
        .collect();
    let applied = apply(dir, changes);
    let verdict = applied.and_then(|()| scan(dir, scanner));
    match verdict {
        Ok(report) => Ok(report),
        Err(why) => {
            restore(dir, existed, &before);
            Err(format!("{why}; nothing was changed"))
        }
    }
}

fn apply(dir: &Path, changes: &[Change]) -> Result<(), String> {
    for change in changes {
        let path = dir.join(change.rel());
        match change {
            Change::Write(_, text) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|e| io(parent, &e))?;
                }
                private_fs::write_atomic(&path, text.as_bytes()).map_err(|e| io(&path, &e))?;
            }
            Change::Remove(_) => fs::remove_file(&path).map_err(|e| io(&path, &e))?,
        }
    }
    Ok(())
}

/// Scans the skill in `dir`: a `dangerous` verdict and a scanner failure
/// are both refusals.
///
/// # Errors
///
/// The refusal text.
pub fn scan(dir: &Path, scanner: &dyn Scanner) -> Result<Report, String> {
    let files: Vec<(String, Vec<u8>)> = layout::walk(dir)
        .into_iter()
        .filter_map(|rel| fs::read(dir.join(&rel)).ok().map(|b| (rel, b)))
        .collect();
    match scanner.scan(&files) {
        Ok(report) if report.verdict == Verdict::Dangerous => Err(format!(
            "security scan refused the write: {}",
            report.summary()
        )),
        Ok(report) => Ok(report),
        Err(e) => Err(format!("security scan failed: {e}")),
    }
}

fn restore(dir: &Path, existed: bool, before: &[(String, Option<Vec<u8>>)]) {
    if !existed {
        let _ = fs::remove_dir_all(dir);
        return;
    }
    for (rel, bytes) in before {
        let path = dir.join(rel);
        let result = match bytes {
            Some(bytes) => private_fs::write_atomic(&path, bytes),
            None => fs::remove_file(&path).or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(e)
                }
            }),
        };
        if let Err(e) = result {
            tracing::error!(path = %path.display(), error = %e, "skill file not restored after a refused write");
        }
    }
}

/// Moves `source` (a skill directory or one file) to
/// `<skills>/.archive/<name>-<timestamp>/<rel>`; returns the new path.
///
/// # Errors
///
/// I/O failure text.
pub fn archive(skills_dir: &Path, name: &str, source: &Path, rel: &str) -> Result<PathBuf, String> {
    let stamp = Timestamp::now().strftime("%Y%m%dT%H%M%S%.3fZ").to_string();
    let base = skills_dir.join(".archive").join(format!("{name}-{stamp}"));
    let target = if rel.is_empty() { base } else { base.join(rel) };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| io(parent, &e))?;
    }
    fs::rename(source, &target).map_err(|e| io(source, &e))?;
    Ok(target)
}

fn io(path: &Path, e: &std::io::Error) -> String {
    format!("{}: {e}", path.display())
}
