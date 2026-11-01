//! Installing a pinned runtime (spec: distribution-and-update, Managed
//! runtime store; Download retry budget; Download mirrors; Persistent
//! runtime opt-outs).
//!
//! 1. Refuse a name `runtimes.exclude` lists, before any request.
//! 2. Download the archive into `.downloads/` under the retry budget,
//!    resuming a partial file an interrupted run left; from
//!    `runtimes.mirrors.runtimes` when set (falling back to the pinned URL
//!    only with `update.mirror_fallback`).
//! 3. Check size and SHA-256 against the pin; on a mismatch delete the
//!    download and fail with `sha256 mismatch for <url>: pinned <expected>,
//!    got <actual>`. A mirror therefore cannot change what is installed.
//! 4. Unpack into `.staging/<id>/`, record every file's digest, and rename
//!    the staging directory to `<name>/<version>-<sha12>/`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_net::OutboundPolicy;

use super::index::{Pin, PinnedArtifact};
use super::{Installed, RECORD_FILE, RECORD_SCHEMA, Record, Store, digest_tree, hash_file};
use crate::download::{self, Budget, FileSink, Stats};

/// How runtimes are downloaded.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    /// `runtimes.mirrors.runtimes`: archives at
    /// `<mirror>/<name>/<version>/<archive file name>`.
    pub mirror: Option<String>,
    /// `update.mirror_fallback`.
    pub fallback: bool,
    /// `runtimes.exclude`.
    pub exclude: Vec<String>,
}

/// Where progress lines go.
pub type Progress = Arc<dyn Fn(&str) + Send + Sync>;

/// Installs runtimes into one store.
pub struct Installer {
    store: Store,
    client: reqwest::Client,
    sources: Sources,
    budget: Budget,
    progress: Progress,
}

impl std::fmt::Debug for Installer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Installer")
            .field("root", &self.store.root())
            .field("sources", &self.sources)
            .finish_non_exhaustive()
    }
}

impl Installer {
    /// An installer for `store`; progress lines go to `progress`.
    ///
    /// # Errors
    ///
    /// `net.client_init` / `net.extra_ca` when the HTTP client cannot be
    /// built.
    pub fn new(
        store: Store,
        policy: &OutboundPolicy,
        sources: Sources,
        progress: Progress,
    ) -> Result<Self, Error> {
        let client = pmt_net::client_builder(policy)?
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_mins(1))
            .build()
            .map_err(|e| Error::new(&codes::NET_CLIENT_INIT, e.to_string()))?;
        Ok(Self {
            store,
            client,
            sources,
            budget: Budget::default(),
            progress,
        })
    }

    /// Replaces the retry budget (tests use short delays).
    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// The store it installs into.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Whether `runtimes.exclude` lists `name`.
    #[must_use]
    pub fn excluded(&self, name: &str) -> bool {
        self.sources.exclude.iter().any(|n| n == name)
    }

    /// Installs the pinned version of `name` (a no-op when installed).
    ///
    /// # Errors
    ///
    /// `runtime.unknown` (2), `runtime.excluded` (1),
    /// `runtime.unsupported` (69), `runtime.download` (69),
    /// `runtime.digest_mismatch` (77), `runtime.io` (74).
    pub async fn install(&self, name: &str) -> Result<Installed, Error> {
        let (pin, artifact) = self.store.pinned(name)?;
        if self.excluded(name) {
            return Err(excluded(name));
        }
        if let Some(installed) = self.store.installed(name) {
            return Ok(installed);
        }
        let partial = self.partial_path(name, pin, artifact)?;
        // One installer per runtime at a time: they would share the
        // partial download. A waiter finds the runtime installed.
        let _lock = lock(&partial.with_extension("lock"))?;
        if let Some(installed) = self.store.installed(name) {
            return Ok(installed);
        }
        let (url, source, stats) = self.download(name, pin, artifact, &partial).await?;
        check_digest(&partial, artifact, &url)?;
        (self.progress)(&format!("unpacking {name} {}", pin.version));
        let record = Record {
            schema: RECORD_SCHEMA.to_owned(),
            name: name.to_owned(),
            version: pin.version.clone(),
            sha256: artifact.sha256.clone(),
            url,
            source: source.to_owned(),
            installed_at: jiff::Timestamp::now().to_string(),
            retries: stats.retries,
            resumed: stats.resumed,
            exe: artifact.exe.clone(),
            files: std::collections::BTreeMap::new(),
        };
        let dir = self.store.root().join(name).join(pin.dir_name(artifact));
        let installed = self.unpack(&partial, artifact, record, &dir)?;
        let _ = std::fs::remove_file(&partial);
        Ok(installed)
    }

    fn partial_path(
        &self,
        name: &str,
        pin: &Pin,
        artifact: &PinnedArtifact,
    ) -> Result<PathBuf, Error> {
        let downloads = self.store.downloads();
        std::fs::create_dir_all(&downloads)
            .map_err(|e| Error::io(&codes::RUNTIME_IO, &downloads, &e))?;
        Ok(downloads.join(format!(
            "{name}-{}-{}.part",
            pin.dir_name(artifact),
            artifact.file_name()
        )))
    }

    /// The URLs to try, in order, with their source label.
    fn urls(
        &self,
        name: &str,
        pin: &Pin,
        artifact: &PinnedArtifact,
    ) -> Vec<(String, &'static str)> {
        let origin = (artifact.url.clone(), "origin");
        match &self.sources.mirror {
            Some(mirror) => {
                let mirrored = format!(
                    "{}/{name}/{}/{}",
                    mirror.trim_end_matches('/'),
                    pin.version,
                    artifact.file_name()
                );
                let mut urls = vec![(mirrored, "mirror")];
                if self.sources.fallback {
                    urls.push(origin);
                }
                urls
            }
            None => vec![origin],
        }
    }

    async fn download(
        &self,
        name: &str,
        pin: &Pin,
        artifact: &PinnedArtifact,
        partial: &Path,
    ) -> Result<(String, &'static str, Stats), Error> {
        let mut last = None;
        for (url, source) in self.urls(name, pin, artifact) {
            let mut sink =
                FileSink::open(partial).map_err(|e| Error::io(&codes::RUNTIME_IO, partial, &e))?;
            if sink_complete(&sink, artifact) {
                return Ok((url, source, Stats::default()));
            }
            (self.progress)(&format!(
                "downloading {name} {} ({} bytes) from {url}",
                pin.version, artifact.size
            ));
            match download::fetch(&self.client, &url, &mut sink, &self.budget).await {
                Ok(stats) => {
                    sink.sync()
                        .map_err(|e| Error::io(&codes::RUNTIME_IO, partial, &e))?;
                    return Ok((url, source, stats));
                }
                Err(reason) => {
                    // Bytes from one source are not resumed from another.
                    let _ = std::fs::remove_file(partial);
                    last = Some(Error::new(
                        &codes::RUNTIME_DOWNLOAD,
                        format!("cannot download {url}: {reason}"),
                    ));
                }
            }
        }
        Err(last.expect("at least one URL is tried"))
    }

    fn unpack(
        &self,
        archive: &Path,
        artifact: &PinnedArtifact,
        mut record: Record,
        dir: &Path,
    ) -> Result<Installed, Error> {
        let staging = self
            .store
            .staging()
            .join(ulid::Ulid::generate().to_string());
        std::fs::create_dir_all(&staging)
            .map_err(|e| Error::io(&codes::RUNTIME_IO, &staging, &e))?;
        let result = (|| {
            super::extract::unpack(archive, artifact.format, &staging).map_err(|reason| {
                Error::new(
                    &codes::RUNTIME_IO,
                    format!("cannot unpack {}: {reason}", artifact.file_name()),
                )
            })?;
            record.files =
                digest_tree(&staging).map_err(|e| Error::io(&codes::RUNTIME_IO, &staging, &e))?;
            let text = serde_json::to_vec_pretty(&record).expect("a record serializes");
            let path = staging.join(RECORD_FILE);
            std::fs::write(&path, text).map_err(|e| Error::io(&codes::RUNTIME_IO, &path, &e))?;
            publish(&staging, dir)
        })();
        let _ = std::fs::remove_dir_all(&staging);
        result?;
        Ok(Installed {
            dir: dir.to_owned(),
            record,
        })
    }
}

/// The error of an excluded runtime.
#[must_use]
pub fn excluded(name: &str) -> Error {
    Error::new(
        &codes::RUNTIME_EXCLUDED,
        format!(
            "{name} is excluded by runtimes.exclude: run 'prometheus runtimes exclude remove {name}' to allow it"
        ),
    )
}

/// Waits for the exclusive lock on `path`; released when the file drops.
fn lock(path: &Path) -> Result<std::fs::File, Error> {
    let file = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| Error::io(&codes::RUNTIME_IO, path, &e))?;
    file.lock()
        .map_err(|e| Error::io(&codes::RUNTIME_IO, path, &e))?;
    Ok(file)
}

/// Whether an earlier run already downloaded the whole archive.
fn sink_complete(sink: &FileSink, artifact: &PinnedArtifact) -> bool {
    use download::Sink as _;
    sink.written() == artifact.size
}

/// Fails, deleting the download, unless it is the pinned archive.
fn check_digest(partial: &Path, artifact: &PinnedArtifact, url: &str) -> Result<(), Error> {
    let actual = hash_file(partial).map_err(|e| Error::io(&codes::RUNTIME_IO, partial, &e))?;
    if actual == artifact.sha256 {
        return Ok(());
    }
    let _ = std::fs::remove_file(partial);
    Err(Error::new(
        &codes::RUNTIME_DIGEST_MISMATCH,
        format!(
            "sha256 mismatch for {url}: pinned {}, got {actual}",
            artifact.sha256
        ),
    )
    .with_hint("the download was deleted and nothing was installed"))
}

/// Renames the verified staging directory into place. Another process may
/// have published the same version meanwhile: its copy is kept.
fn publish(staging: &Path, dir: &Path) -> Result<(), Error> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(&codes::RUNTIME_IO, parent, &e))?;
    }
    match std::fs::rename(staging, dir) {
        Ok(()) => Ok(()),
        Err(_) if dir.join(RECORD_FILE).is_file() => Ok(()),
        Err(e) => Err(Error::io(&codes::RUNTIME_IO, dir, &e)),
    }
}
