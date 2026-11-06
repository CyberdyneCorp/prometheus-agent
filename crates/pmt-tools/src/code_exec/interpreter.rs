//! Which Python a script runs on (spec: subagent-delegation,
//! `execute_code` execution modes): in `project` mode the virtual
//! environment active in the session's terminal environment, or a `.venv`
//! in the working directory, when its interpreter runs and is 3.8 or
//! newer; otherwise (and always in `strict` mode) the managed runtime. A
//! project environment that exists but cannot be used is a fallback the
//! result notes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use pmt_sandbox::Sandbox;

/// The oldest project Python used.
pub const MIN_VERSION: (u32, u32) = (3, 8);

/// How long the version probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The project's virtual environment as the session sees it.
#[derive(Debug, Clone)]
pub struct Project<'a> {
    /// `VIRTUAL_ENV` of the session's terminal environment.
    pub active: Option<&'a Path>,
    /// The session's working directory.
    pub workdir: &'a Path,
}

/// The chosen project interpreter, or why the managed runtime runs
/// instead (`None`: there is no project environment at all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Use this interpreter.
    Use(PathBuf),
    /// No project environment.
    None,
    /// The project environment cannot be used: the note for the result.
    Fallback(String),
}

/// The project interpreter for `project`, probed under `sandbox`.
pub async fn project(project: &Project<'_>, sandbox: &Sandbox) -> Choice {
    let Some(venv) = venv_dir(project) else {
        return Choice::None;
    };
    let python = interpreter_of(&venv);
    let why = match version(&python, sandbox).await {
        Ok(found) if found >= MIN_VERSION => return Choice::Use(python),
        Ok((major, minor)) => format!(
            "it is Python {major}.{minor}; {}.{} or newer is needed",
            MIN_VERSION.0, MIN_VERSION.1
        ),
        Err(why) => why,
    };
    Choice::Fallback(format!(
        "The project virtual environment {} cannot be used ({why}); the script ran on the \
         managed Python runtime.",
        venv.display()
    ))
}

/// The active environment, else `.venv` in the working directory.
fn venv_dir(project: &Project<'_>) -> Option<PathBuf> {
    if let Some(active) = project.active.filter(|p| !p.as_os_str().is_empty()) {
        return Some(active.to_path_buf());
    }
    let local = project.workdir.join(".venv");
    local.join("pyvenv.cfg").is_file().then_some(local)
}

/// The interpreter of a virtual environment.
#[must_use]
pub fn interpreter_of(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

type Probed = (Option<SystemTime>, Result<(u32, u32), String>);

/// Probe results by interpreter, valid while its modification time holds.
static PROBED: LazyLock<Mutex<HashMap<PathBuf, Probed>>> = LazyLock::new(Mutex::default);

/// The interpreter's `(major, minor)`, or why it cannot run.
async fn version(python: &Path, sandbox: &Sandbox) -> Result<(u32, u32), String> {
    let Ok(meta) = std::fs::metadata(python) else {
        return Err(format!("its interpreter {} is missing", python.display()));
    };
    let stamp = meta.modified().ok();
    if let Some((at, found)) = PROBED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(python)
        && *at == stamp
    {
        return found.clone();
    }
    let found = probe(python, sandbox).await;
    PROBED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(python.to_path_buf(), (stamp, found.clone()));
    found
}

async fn probe(python: &Path, sandbox: &Sandbox) -> Result<(u32, u32), String> {
    let cannot = |why: String| format!("its interpreter {} {why}", python.display());
    let mut command = sandbox
        .command(python)
        .map_err(|e| cannot(format!("cannot be sandboxed: {}", e.message())))?;
    command
        .args(["-E", "-s", "-c"])
        .arg("import sys; print('%d %d' % sys.version_info[:2])")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let output = tokio::time::timeout(PROBE_TIMEOUT, command.output())
        .await
        .map_err(|_| cannot("did not answer".to_owned()))?
        .map_err(|e| cannot(format!("does not run: {e}")))?;
    if !output.status.success() {
        return Err(cannot("does not run".to_owned()));
    }
    parse_version(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| cannot("reported no version".to_owned()))
}

fn parse_version(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.split_whitespace();
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_active_environment_wins_over_a_local_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".venv")).unwrap();
        std::fs::write(dir.path().join(".venv/pyvenv.cfg"), "home = /usr\n").unwrap();
        let active = dir.path().join("active");
        let found = venv_dir(&Project {
            active: Some(&active),
            workdir: dir.path(),
        });
        assert_eq!(found, Some(active));
        let found = venv_dir(&Project {
            active: None,
            workdir: dir.path(),
        });
        assert_eq!(found, Some(dir.path().join(".venv")));
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            venv_dir(&Project {
                active: None,
                workdir: empty.path()
            }),
            None,
            "a directory without pyvenv.cfg is no environment"
        );
    }

    #[test]
    fn versions_parse_from_the_probe_line() {
        assert_eq!(parse_version("3 12\n"), Some((3, 12)));
        assert_eq!(parse_version("x"), None);
        assert!((3, 7) < MIN_VERSION && (3, 8) >= MIN_VERSION);
    }
}
