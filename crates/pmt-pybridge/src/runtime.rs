//! The managed CPython and uv (spec: python-bridge, Managed CPython
//! runtime; distribution-and-update, Managed runtime store).
//!
//! uv is a pinned runtime of the store (`<home>/runtimes/uv/...`, verified
//! against the index compiled into this binary). CPython is provisioned by
//! that uv into `<home>/runtimes/python/` (`uv python install`), which
//! checks every interpreter download against the SHA-256 manifest uv
//! carries, so the interpreter is as pinned as the uv build. A system
//! Python is never used.

use std::path::{Path, PathBuf};
use std::process::Command;

use pmt_config::schema::PythonVersion;
use pmt_core::error::{Error, codes};

/// A managed interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interpreter {
    /// The executable.
    pub exe: PathBuf,
    /// Its install directory (`cpython-3.12.x-<platform>`).
    pub dir: PathBuf,
    /// The full version (`3.12.11`).
    pub version: String,
}

/// `<home>/runtimes/python`: where uv installs managed interpreters.
#[must_use]
pub fn python_root(home: &Path) -> PathBuf {
    home.join("runtimes").join("python")
}

/// `<home>/cache/uv`: the package cache every environment builds from.
#[must_use]
pub fn uv_cache(home: &Path) -> PathBuf {
    home.join("cache").join("uv")
}

/// The managed uv, when installed.
#[must_use]
pub fn uv(home: &Path) -> Option<PathBuf> {
    pmt_update::runtimes::Store::open(home)
        .installed("uv")
        .map(|installed| installed.exe())
        .filter(|exe| exe.exists())
}

/// The newest installed interpreter of `version`.
#[must_use]
pub fn find(home: &Path, version: PythonVersion) -> Option<Interpreter> {
    let prefix = format!("cpython-{}.", version.as_str());
    let entries = std::fs::read_dir(python_root(home)).ok()?;
    let mut found: Vec<Interpreter> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rest = name.strip_prefix(&prefix)?;
            let patch: String = rest.chars().take_while(char::is_ascii_digit).collect();
            let dir = entry.path();
            let exe = executable(&dir);
            exe.exists().then(|| Interpreter {
                version: format!("{}.{patch}", version.as_str()),
                exe,
                dir,
            })
        })
        .collect();
    found.sort_by_key(|i| patch_of(&i.version));
    found.pop()
}

fn patch_of(version: &str) -> u64 {
    version
        .rsplit('.')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
}

/// The interpreter inside an install directory.
#[must_use]
pub fn executable(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        dir.join("python.exe")
    } else {
        dir.join("bin").join("python3")
    }
}

/// The error every bridged plugin fails with when the interpreter is
/// missing.
#[must_use]
pub fn missing(version: PythonVersion) -> Error {
    let v = version.as_str();
    Error::new(
        &codes::BRIDGE_RUNTIME_MISSING,
        format!(
            "Python runtime {v} is not installed and could not be downloaded; run prometheus runtimes install python@{v}"
        ),
    )
}

/// The installed interpreter of `version`.
///
/// # Errors
///
/// [`missing`] when it is not installed.
pub fn require(home: &Path, version: PythonVersion) -> Result<Interpreter, Error> {
    find(home, version).ok_or_else(|| missing(version))
}

/// The managed uv as the bridge runs it: no user or project uv
/// configuration, the shared package cache, and interpreters only from the
/// managed directory. uv's own index variables (`UV_INDEX_URL`,
/// `UV_FIND_LINKS`...) still apply.
#[derive(Debug, Clone)]
pub struct Uv {
    /// The executable.
    pub exe: PathBuf,
    /// The Prometheus home (cache and interpreter directories).
    pub home: PathBuf,
    /// Extra variables for every uv run.
    pub env: Vec<(String, String)>,
}

impl Uv {
    /// The managed uv of `home`, when installed.
    #[must_use]
    pub fn of(home: &Path) -> Option<Self> {
        uv(home).map(|exe| Self::new(exe, home))
    }

    /// `exe` over `home`.
    #[must_use]
    pub fn new(exe: PathBuf, home: &Path) -> Self {
        Self {
            exe,
            home: home.to_path_buf(),
            env: Vec::new(),
        }
    }

    /// A command running it.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.exe);
        command
            .env("UV_NO_CONFIG", "1")
            .env("UV_CACHE_DIR", uv_cache(&self.home))
            .env("UV_PYTHON_INSTALL_DIR", python_root(&self.home))
            .env("UV_PYTHON_PREFERENCE", "only-managed")
            .env("UV_PYTHON_DOWNLOADS", "manual")
            .env("UV_PYTHON_INSTALL_REGISTRY", "0")
            .env_remove("VIRTUAL_ENV")
            .env_remove("PYTHONPATH")
            .env_remove("PYTHONHOME");
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }
}

/// Installs CPython `version` with `uv` into the managed directory
/// (network access; `prometheus runtimes install python@<v>`).
///
/// # Errors
///
/// [`missing`] with uv's output when the download fails.
pub fn install(uv: &Uv, version: PythonVersion) -> Result<Interpreter, Error> {
    let home = uv.home.as_path();
    if let Some(found) = find(home, version) {
        return Ok(found);
    }
    let output = uv
        .command()
        .args([
            "python",
            "install",
            version.as_str(),
            "--no-bin",
            "--install-dir",
        ])
        .arg(python_root(home))
        .output()
        .map_err(|e| missing(version).with_hint(format!("could not run uv: {e}")))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(missing(version).with_hint(detail));
    }
    require(home, version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_patch_of_the_wanted_version_is_found() {
        let home = tempfile::tempdir().unwrap();
        for name in [
            "cpython-3.12.3-macos-aarch64-none",
            "cpython-3.12.11-macos-aarch64-none",
            "cpython-3.13.1-x",
        ] {
            let exe = executable(&python_root(home.path()).join(name));
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(&exe, "").unwrap();
        }
        let found = find(home.path(), PythonVersion::V3_12).unwrap();
        assert_eq!(found.version, "3.12.11");
        assert!(find(home.path(), PythonVersion::V3_11).is_none());
    }

    #[test]
    fn a_missing_runtime_names_the_install_command() {
        let home = tempfile::tempdir().unwrap();
        let err = require(home.path(), PythonVersion::V3_12).unwrap_err();
        assert_eq!(
            err.message(),
            "Python runtime 3.12 is not installed and could not be downloaded; run prometheus runtimes install python@3.12"
        );
    }
}
