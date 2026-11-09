//! The llama.cpp engine (spec: local-models, Hash-pinned engine
//! installation):
//!
//! ```text
//! <home>/runtimes/llamacpp/<version>-<backend>/   an unpacked, verified build
//!     .prometheus-engine.json                      its record
//! <home>/runtimes/llamacpp/.archives/              verified archives, before unpacking
//! <home>/runtimes/llamacpp/.staging/               unpacking, before the rename
//! ```
//!
//! Every archive is downloaded through the local-models downloader, checked
//! against [`index`] and only then unpacked; the build directory appears by
//! an atomic rename. Nothing here runs at boot or session start: an
//! installed engine is used as it is, and a newer pin is installed only by
//! `prometheus local install` or `update`.

pub mod index;

use std::path::{Path, PathBuf};

use pmt_config::daemon_local::LocalBackend;
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::download::{Control, Downloader, FileSpec, Finished, Jobs, Progress, io_error};
pub use index::{Build, EngineFile, EngineIndex};

/// The record inside every build directory.
pub const RECORD_FILE: &str = ".prometheus-engine.json";

/// What an installed build records about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineRecord {
    /// The llama.cpp release.
    pub version: String,
    /// Its backend.
    pub backend: LocalBackend,
    /// The platform it was installed for.
    pub platform: String,
    /// `llama-server`, relative to the build directory.
    pub exe: String,
    /// Shared-library directories, relative.
    #[serde(default)]
    pub library_dirs: Vec<String>,
    /// SHA-256 of each archive.
    pub archives: Vec<String>,
    /// RFC 3339 install time.
    pub installed_at: String,
}

/// One installed build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// Its directory.
    pub dir: PathBuf,
    /// Its record.
    pub record: EngineRecord,
}

impl Installed {
    /// `llama-server`.
    #[must_use]
    pub fn exe(&self) -> PathBuf {
        self.dir.join(&self.record.exe)
    }

    /// The directories the dynamic loader must search.
    #[must_use]
    pub fn library_dirs(&self) -> Vec<PathBuf> {
        self.record
            .library_dirs
            .iter()
            .map(|d| self.dir.join(d))
            .collect()
    }
}

/// How an install run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The build is installed.
    Installed(Installed),
    /// The download was paused; the job id resumes it.
    Paused(String),
}

/// The engine store of one home.
#[derive(Debug, Clone)]
pub struct Engine {
    root: PathBuf,
    index: EngineIndex,
}

impl Engine {
    /// The store of `home` with this binary's pins.
    #[must_use]
    pub fn open(home: &Path) -> Self {
        Self::new(
            home.join("runtimes").join("llamacpp"),
            EngineIndex::embedded(),
        )
    }

    /// A store at `root` pinned by `index` (tests pin fixture archives).
    #[must_use]
    pub fn new(root: PathBuf, index: EngineIndex) -> Self {
        Self { root, index }
    }

    /// The pins.
    #[must_use]
    pub fn index(&self) -> &EngineIndex {
        &self.index
    }

    /// The pinned build of `backend`, when installed.
    #[must_use]
    pub fn installed(&self, backend: LocalBackend) -> Option<Installed> {
        read(&self.root.join(self.dir_name(backend)))
    }

    /// Every installed build, pinned or not.
    #[must_use]
    pub fn installed_all(&self) -> Vec<Installed> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut all: Vec<Installed> = entries
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .filter_map(|e| read(&e.path()))
            .collect();
        all.sort_by(|a, b| a.dir.cmp(&b.dir));
        all
    }

    /// Downloads, verifies and unpacks the pinned build of `backend` for
    /// `platform` (a no-op when installed). The pair is checked before any
    /// request.
    ///
    /// # Errors
    ///
    /// `runtime.unsupported` (69) for a pair without a build,
    /// `local.digest_mismatch` (74) naming the archive (deleted; nothing
    /// installed), `runtime.download` (69), `runtime.io` (74).
    pub async fn install(
        &self,
        platform: &str,
        backend: LocalBackend,
        downloader: &Downloader,
        jobs: &Jobs,
        control: &Control,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Outcome, Error> {
        let build = self.index.build(platform, backend)?;
        if let Some(installed) = self.installed(backend) {
            return Ok(Outcome::Installed(installed));
        }
        let subject = format!("engine {}", self.dir_name(backend));
        let mut job = match jobs.unfinished(&subject) {
            Some(job) => job,
            None => jobs.create(&subject, self.files(build), serde_json::Value::Null)?,
        };
        match downloader.run(jobs, &mut job, control, progress).await {
            Ok(Finished::Paused) => return Ok(Outcome::Paused(job.id)),
            Ok(Finished::Done) => {}
            Err(err) => {
                // A mismatch deleted its archive: nothing is left to resume.
                if err.code() == &codes::LOCAL_DIGEST_MISMATCH {
                    jobs.remove(&job)?;
                }
                return Err(err);
            }
        }
        let installed = self.unpack(platform, backend, build)?;
        for file in &build.files {
            let _ = std::fs::remove_file(self.archive(file));
        }
        jobs.remove(&job)?;
        Ok(Outcome::Installed(installed))
    }

    /// Removes every installed build but `keep` and `in_use`; returns the
    /// removed directories.
    ///
    /// # Errors
    ///
    /// `runtime.io`.
    pub fn remove_others(&self, keep: &Path, in_use: Option<&Path>) -> Result<Vec<PathBuf>, Error> {
        let mut removed = Vec::new();
        for build in self.installed_all() {
            if build.dir == keep || Some(build.dir.as_path()) == in_use {
                continue;
            }
            std::fs::remove_dir_all(&build.dir).map_err(|e| io_error(&build.dir, &e))?;
            removed.push(build.dir);
        }
        Ok(removed)
    }

    /// `<version>-<backend>`.
    #[must_use]
    pub fn dir_name(&self, backend: LocalBackend) -> String {
        format!("{}-{backend}", self.index.version)
    }

    fn files(&self, build: &Build) -> Vec<FileSpec> {
        build
            .files
            .iter()
            .map(|file| FileSpec {
                name: file.file_name().to_owned(),
                url: file.url.clone(),
                size: file.size,
                sha256: file.sha256.clone(),
                target: self.archive(file),
            })
            .collect()
    }

    fn archive(&self, file: &EngineFile) -> PathBuf {
        self.root
            .join(".archives")
            .join(format!("{}-{}", &file.sha256[..12], file.file_name()))
    }

    fn unpack(
        &self,
        platform: &str,
        backend: LocalBackend,
        build: &Build,
    ) -> Result<Installed, Error> {
        let staging = self
            .root
            .join(".staging")
            .join(ulid::Ulid::generate().to_string());
        std::fs::create_dir_all(&staging).map_err(|e| io_error(&staging, &e))?;
        let record = EngineRecord {
            version: self.index.version.clone(),
            backend,
            platform: platform.to_owned(),
            exe: build.exe.clone(),
            library_dirs: build.library_dirs.clone(),
            archives: build.files.iter().map(|f| f.sha256.clone()).collect(),
            installed_at: jiff::Timestamp::now().to_string(),
        };
        let dir = self.root.join(self.dir_name(backend));
        let result = (|| {
            for file in &build.files {
                pmt_update::runtimes::extract::unpack(&self.archive(file), file.format, &staging)
                    .map_err(|reason| {
                    Error::new(
                        &codes::RUNTIME_IO,
                        format!("cannot unpack {}: {reason}", file.file_name()),
                    )
                })?;
            }
            let path = staging.join(RECORD_FILE);
            let text = serde_json::to_vec_pretty(&record).expect("a record serializes");
            std::fs::write(&path, text).map_err(|e| io_error(&path, &e))?;
            match std::fs::rename(&staging, &dir) {
                Ok(()) => Ok(()),
                // Another process published the same build meanwhile.
                Err(_) if dir.join(RECORD_FILE).is_file() => Ok(()),
                Err(e) => Err(io_error(&dir, &e)),
            }
        })();
        let _ = std::fs::remove_dir_all(&staging);
        result?;
        Ok(Installed { dir, record })
    }
}

fn read(dir: &Path) -> Option<Installed> {
    let text = std::fs::read_to_string(dir.join(RECORD_FILE)).ok()?;
    Some(Installed {
        dir: dir.to_owned(),
        record: serde_json::from_str(&text).ok()?,
    })
}
