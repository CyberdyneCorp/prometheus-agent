//! Where the demo runs: `--output DIR` or a fresh temporary directory,
//! holding the demo's own Prometheus home, the ledger repository and the
//! mock script. Without `--keep` everything the demo created is removed
//! afterwards; a directory the user named is emptied, never deleted.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};

/// The demo's directory.
pub struct Place {
    root: PathBuf,
    /// Set for a fresh temporary directory; removed on drop unless kept.
    temp: Option<tempfile::TempDir>,
    /// `--output DIR` did not exist and the demo created it.
    created: bool,
}

impl Place {
    /// `--output DIR` (missing or empty) or a fresh temporary directory.
    ///
    /// # Errors
    ///
    /// `demo.output_not_empty` (2), `demo.io` (74).
    pub fn prepare(output: Option<&Path>) -> Result<Self, Error> {
        let Some(dir) = output else {
            let temp = tempfile::Builder::new()
                .prefix("prometheus-demo-")
                .tempdir()
                .map_err(|e| io_error("cannot create a temporary directory", &e))?;
            return Ok(Self {
                root: temp.path().to_path_buf(),
                temp: Some(temp),
                created: true,
            });
        };
        // The session and its tools run with this as their working
        // directory, so it must not depend on the process's.
        let dir = &std::path::absolute(dir)
            .map_err(|e| io_error(&format!("cannot resolve {}", dir.display()), &e))?;
        let created = !dir.exists();
        if !created && !is_empty_dir(dir)? {
            return Err(Error::new(
                &codes::DEMO_OUTPUT_NOT_EMPTY,
                format!("{} is not an empty directory", dir.display()),
            )
            .with_hint("pass --output with a new or empty directory"));
        }
        std::fs::create_dir_all(dir)
            .map_err(|e| io_error(&format!("cannot create {}", dir.display()), &e))?;
        Ok(Self {
            root: dir.clone(),
            temp: None,
            created,
        })
    }

    /// The directory itself.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The demo's Prometheus home.
    #[must_use]
    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// The ledger repository the agent works in.
    #[must_use]
    pub fn repository(&self) -> PathBuf {
        self.root.join("ledger")
    }

    /// The mock provider's script.
    #[must_use]
    pub fn mock_script(&self) -> PathBuf {
        self.root.join("mock.toml")
    }

    /// Keeps the directory, or removes what the demo created in it. A
    /// failed removal is reported on stderr: the run's verdict stands.
    pub fn finish(self, keep: bool) {
        if keep {
            if let Some(temp) = self.temp {
                // Ends the guard without removing the directory.
                let _ = temp.keep();
            }
            return;
        }
        let removed = match (self.temp, self.created) {
            (Some(temp), _) => temp.close(),
            (None, true) => std::fs::remove_dir_all(&self.root),
            (None, false) => empty(&self.root),
        };
        if let Err(e) = removed {
            eprintln!("warning: cannot remove {}: {e}", self.root.display());
        }
    }
}

fn is_empty_dir(dir: &Path) -> Result<bool, Error> {
    let mut entries = std::fs::read_dir(dir)
        .map_err(|e| io_error(&format!("cannot read {}", dir.display()), &e))?;
    Ok(entries.next().is_none())
}

/// Removes every entry of `dir`, which was empty when the demo started.
fn empty(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn io_error(what: &str, err: &std::io::Error) -> Error {
    Error::new(&codes::DEMO_IO, format!("{what}: {err}"))
}
