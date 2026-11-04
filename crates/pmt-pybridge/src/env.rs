//! Pinned Python environments (spec: python-bridge, Per-plugin pinned
//! environments; Python skill scripts; Named side environments).
//!
//! Requirements (a `requirements.txt`, `pyproject.toml`
//! `[project].dependencies`, or a manifest's inline list) are resolved by
//! uv into `bridge.lock`: exact versions with hashes. Resolution needs the
//! network and happens only in `plugins install`/`update` (and `bridge env`).
//! An environment is a virtualenv at `<root>/<name>-<lock_digest12>/`
//! installed from the lock with `--require-hashes`; it is marked complete
//! only after the install succeeds and is reused while its lock digest and
//! interpreter version stay the same, without running uv at all. Building
//! it again (a new interpreter, a deleted directory) runs offline against
//! the package cache that the resolution filled.

use std::path::{Path, PathBuf};
use std::process::Command;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::runtime::{Interpreter, Uv};

/// The lock file name.
pub const LOCK_FILE: &str = "bridge.lock";
/// The completion marker inside an environment.
const MARKER: &str = ".prometheus-env.json";

/// Reads a plugin's or skill's requirements: `requirements.txt`, else
/// `pyproject.toml`'s `[project].dependencies`, plus `inline`.
///
/// # Errors
///
/// `bridge.requirements` for an unreadable file or an unsupported option
/// line.
pub fn requirements(dir: &Path, inline: &[String]) -> Result<Vec<String>, Error> {
    let mut out: Vec<String> = Vec::new();
    let requirements_txt = dir.join("requirements.txt");
    let pyproject = dir.join("pyproject.toml");
    if requirements_txt.is_file() {
        let text = std::fs::read_to_string(&requirements_txt)
            .map_err(|e| Error::io(&codes::BRIDGE_REQUIREMENTS, &requirements_txt, &e))?;
        out.extend(parse_requirements(&text)?);
    } else if pyproject.is_file() {
        out.extend(pyproject_dependencies(&pyproject)?);
    }
    out.extend(inline.iter().cloned());
    out.dedup();
    Ok(out)
}

/// The requirement lines of a `requirements.txt` (comments dropped).
///
/// # Errors
///
/// `bridge.requirements` for an option line (`-r`, `-e`, `--hash`...).
pub fn parse_requirements(text: &str) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split(" #").next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('-') {
            return Err(Error::new(
                &codes::BRIDGE_REQUIREMENTS,
                format!("requirements.txt: unsupported option line '{line}' (list packages only)"),
            ));
        }
        out.push(line.to_owned());
    }
    Ok(out)
}

fn pyproject_dependencies(path: &Path) -> Result<Vec<String>, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::io(&codes::BRIDGE_REQUIREMENTS, path, &e))?;
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        Error::new(
            &codes::BRIDGE_REQUIREMENTS,
            format!("pyproject.toml: {}", e.message()),
        )
    })?;
    Ok(table
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

/// Refuses requirements without an upper bound (`==`, `===`, `~=` or a
/// `<`/`<=` clause) unless `allow_unbounded`.
///
/// # Errors
///
/// `bridge.requirements` naming the first unbounded requirement.
pub fn check_bounds(requirements: &[String], allow_unbounded: bool) -> Result<(), Error> {
    if allow_unbounded {
        return Ok(());
    }
    for requirement in requirements {
        let spec = requirement.split(';').next().unwrap_or("");
        let bounded = ["==", "~=", "<"].iter().any(|op| spec.contains(op));
        if !bounded {
            return Err(Error::new(
                &codes::BRIDGE_REQUIREMENTS,
                format!(
                    "requirement '{requirement}' has no upper bound; pin it (e.g. 'name>=1.2,<2') or set python_bridge.allow_unbounded_requirements = true"
                ),
            ));
        }
    }
    Ok(())
}

/// Lower-case hex SHA-256 of a lock's text.
#[must_use]
pub fn lock_digest(text: &str) -> String {
    crate::hex(&Sha256::digest(text.as_bytes()))
}

/// Resolves `requirements` into `<lock_dir>/bridge.lock` with exact
/// versions and hashes for `python` (network access). Returns the lock's
/// path.
///
/// # Errors
///
/// `bridge.environment` with uv's output.
pub fn lock(
    uv: &Uv,
    python: &Interpreter,
    requirements: &[String],
    lock_dir: &Path,
) -> Result<PathBuf, Error> {
    let io = |path: &Path, e: std::io::Error| Error::io(&codes::BRIDGE_ENVIRONMENT, path, &e);
    std::fs::create_dir_all(lock_dir).map_err(|e| io(lock_dir, e))?;
    let lock = lock_dir.join(LOCK_FILE);
    if requirements.is_empty() {
        std::fs::write(&lock, "# no requirements\n").map_err(|e| io(&lock, e))?;
        return Ok(lock);
    }
    let input = lock_dir.join("requirements.in");
    std::fs::write(&input, requirements.join("\n") + "\n").map_err(|e| io(&input, e))?;
    let mut command = uv.command();
    command
        .args([
            "pip",
            "compile",
            "--generate-hashes",
            "--no-header",
            "--quiet",
            "--python",
        ])
        .arg(&python.exe)
        .arg("--output-file")
        .arg(&lock)
        .arg(&input);
    run(&mut command, "resolve the requirements")?;
    Ok(lock)
}

/// What a complete environment records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    lock_digest: String,
    python: String,
}

/// A built environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    /// Its directory.
    pub dir: PathBuf,
    /// Its interpreter.
    pub python: PathBuf,
    /// Its `bin` (`Scripts` on Windows) directory.
    pub bin: PathBuf,
}

impl Env {
    fn at(dir: PathBuf) -> Self {
        let bin = if cfg!(windows) {
            dir.join("Scripts")
        } else {
            dir.join("bin")
        };
        let python = bin.join(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        });
        Self { dir, python, bin }
    }
}

/// `<root>/<name>-<digest12>` for a lock.
#[must_use]
pub fn env_dir(root: &Path, name: &str, lock_text: &str) -> PathBuf {
    root.join(format!("{name}-{}", &lock_digest(lock_text)[..12]))
}

/// The environment of `lock` when it is complete for `python`.
#[must_use]
pub fn existing(root: &Path, name: &str, lock: &Path, python: &Interpreter) -> Option<Env> {
    let text = std::fs::read_to_string(lock).ok()?;
    let dir = env_dir(root, name, &text);
    let marker: Marker = serde_json::from_slice(&std::fs::read(dir.join(MARKER)).ok()?).ok()?;
    let env = Env::at(dir);
    (marker.lock_digest == lock_digest(&text)
        && marker.python == python.version
        && env.python.exists())
    .then_some(env)
}

/// The environment of `lock`, built when missing or stale; `offline`
/// installs only from the package cache.
///
/// # Errors
///
/// `bridge.environment` with uv's output.
pub fn ensure(
    uv: &Uv,
    python: &Interpreter,
    root: &Path,
    name: &str,
    lock: &Path,
    offline: bool,
) -> Result<Env, Error> {
    if let Some(env) = existing(root, name, lock, python) {
        return Ok(env);
    }
    let io = |path: &Path, e: std::io::Error| Error::io(&codes::BRIDGE_ENVIRONMENT, path, &e);
    let text = std::fs::read_to_string(lock).map_err(|e| io(lock, e))?;
    let env = Env::at(env_dir(root, name, &text));
    std::fs::create_dir_all(root).map_err(|e| io(root, e))?;
    let guard_path = root.join(format!(".{name}.lock"));
    let guard = std::fs::File::create(&guard_path).map_err(|e| io(&guard_path, e))?;
    guard.lock().map_err(|e| io(&guard_path, e))?;
    if let Some(done) = existing(root, name, lock, python) {
        return Ok(done);
    }
    // A partial directory (an interrupted build) is rebuilt from scratch.
    let _ = std::fs::remove_dir_all(&env.dir);
    let mut venv = uv.command();
    venv.args(["venv", "--quiet", "--python"])
        .arg(&python.exe)
        .arg(&env.dir);
    run(&mut venv, "create the environment")?;
    let mut sync = uv.command();
    sync.args(["pip", "sync", "--quiet", "--require-hashes", "--python"])
        .arg(&env.python)
        .arg(lock);
    if offline {
        sync.arg("--offline");
    }
    run(&mut sync, "install the locked packages")?;
    let marker = Marker {
        lock_digest: lock_digest(&text),
        python: python.version.clone(),
    };
    let marker_path = env.dir.join(MARKER);
    std::fs::write(
        &marker_path,
        serde_json::to_vec(&marker).unwrap_or_default(),
    )
    .map_err(|e| io(&marker_path, e))?;
    Ok(env)
}

/// A package the environment holds at another version than its lock, or
/// not at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// The package.
    pub package: String,
    /// The locked version.
    pub locked: String,
    /// The installed version (`None`: missing).
    pub installed: Option<String>,
}

/// Compares an environment's installed packages with its lock (`bridge
/// doctor`).
///
/// # Errors
///
/// `bridge.environment` when uv cannot list the packages.
pub fn verify(uv: &Uv, env: &Env, lock: &Path) -> Result<Vec<Mismatch>, Error> {
    let text = std::fs::read_to_string(lock)
        .map_err(|e| Error::io(&codes::BRIDGE_ENVIRONMENT, lock, &e))?;
    let mut freeze = uv.command();
    freeze.args(["pip", "freeze", "--python"]).arg(&env.python);
    let installed = pins(&run(&mut freeze, "list the installed packages")?);
    Ok(pins(&text)
        .into_iter()
        .filter_map(|(package, locked)| {
            let have = installed
                .iter()
                .find(|(p, _)| *p == package)
                .map(|(_, v)| v.clone());
            (have.as_deref() != Some(locked.as_str())).then_some(Mismatch {
                package,
                locked,
                installed: have,
            })
        })
        .collect())
}

/// `name==version` lines, names normalized (PEP 503).
fn pins(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim().trim_end_matches('\\').trim();
            let (name, version) = line.split_once("==")?;
            let name = name
                .split('[')
                .next()?
                .trim()
                .to_ascii_lowercase()
                .replace(['_', '.'], "-");
            let version = version.split([' ', ';']).next()?.trim().to_owned();
            (!name.starts_with('#')).then_some((name, version))
        })
        .collect()
}

/// Runs a uv command, returning its stdout.
fn run(command: &mut Command, what: &str) -> Result<String, Error> {
    let output = command.output().map_err(|e| {
        Error::new(
            &codes::BRIDGE_ENVIRONMENT,
            format!("could not {what}: uv did not run: {e}"),
        )
    })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    Err(Error::new(
        &codes::BRIDGE_ENVIRONMENT,
        format!("could not {what}: {}", detail.trim()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirements_without_an_upper_bound_are_refused() {
        let ok = [
            "requests>=2.31,<3".to_owned(),
            "x==1.0".to_owned(),
            "y~=1.2".to_owned(),
        ];
        check_bounds(&ok, false).unwrap();
        let bad = ["requests>=2.31; python_version < '3.13'".to_owned()];
        let err = check_bounds(&bad, false).unwrap_err();
        assert!(
            err.message().contains("has no upper bound"),
            "{}",
            err.message()
        );
        check_bounds(&bad, true).unwrap();
    }

    #[test]
    fn requirements_come_from_the_plugin_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname='x'\ndependencies=['a==1']\n",
        )
        .unwrap();
        assert_eq!(
            requirements(dir.path(), &["b<2".to_owned()]).unwrap(),
            ["a==1", "b<2"]
        );
        std::fs::write(dir.path().join("requirements.txt"), "# c\nc==2  # pinned\n").unwrap();
        assert_eq!(requirements(dir.path(), &[]).unwrap(), ["c==2"]);
        std::fs::write(dir.path().join("requirements.txt"), "-e .\n").unwrap();
        assert!(requirements(dir.path(), &[]).is_err());
    }

    #[test]
    fn freeze_and_lock_lines_compare_by_normalized_name() {
        let lock =
            "requests==2.32.3 \\\n    --hash=sha256:abc\nmy_pkg==1.0 ; python_version >= '3'\n";
        assert_eq!(
            pins(lock),
            [
                ("requests".to_owned(), "2.32.3".to_owned()),
                ("my-pkg".to_owned(), "1.0".to_owned())
            ]
        );
    }
}
