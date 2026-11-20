//! The content-addressed caches (spec: harness-evolution, Resume and
//! content-addressed session cache). An improver session is keyed by
//! SHA-256 over its label, mission, system prompt, model, prompt version
//! and every evidence file's path and bytes; a rollout by the task
//! fingerprint, harness fingerprint, model, label and seed. A hit costs
//! nothing. A cached session whose saved workspace no longer matches the
//! fingerprint recorded with it (edited on disk) is a miss, so it and
//! everything whose evidence depends on it run again.

use std::path::{Path, PathBuf};

use pmt_config::harness::Tree;
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::ports::RolloutOutcome;
use super::prompts::PROMPT_VERSION;

/// `<profile>/evolve/cache`.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

/// What identifies an improver session.
#[derive(Debug, Clone, Copy)]
pub struct SessionKey<'a> {
    /// The stage label.
    pub label: &'a str,
    /// The mission.
    pub mission: &'a str,
    /// The system prompt.
    pub system: &'a str,
    /// The improver model.
    pub model: &'a str,
    /// The evidence directory (the session's private workspace).
    pub evidence: &'a Path,
}

/// `meta.json` of a cached session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// The stage label.
    pub label: String,
    /// The improver model.
    pub model: String,
    /// `evolve.prompt_version`.
    pub prompt_version: String,
    /// What the session cost when it ran.
    pub usd: f64,
    /// Fingerprint of the saved `workspace/` (the edited harness).
    pub workspace: String,
}

/// A cached session.
#[derive(Debug, Clone)]
pub struct CachedSession {
    /// Its reply.
    pub response: String,
    /// `workspace/`: the harness tree it left.
    pub workspace: PathBuf,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    hash.update(bytes);
}

/// Every file under `root`, relative and sorted.
pub(crate) fn files_under(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => walk(root, &path, out),
                Ok(kind) if kind.is_file() => out.push(
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                ),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// The fingerprint of a directory tree (paths and bytes).
#[must_use]
pub fn dir_fingerprint(root: &Path) -> String {
    let mut hash = Sha256::new();
    for relative in files_under(root) {
        field(&mut hash, relative.as_bytes());
        field(
            &mut hash,
            &std::fs::read(root.join(&relative)).unwrap_or_default(),
        );
    }
    hex(&hash.finalize())
}

impl Cache {
    /// The cache of the profile at `profile_dir`.
    #[must_use]
    pub fn of(profile_dir: &Path) -> Self {
        Self {
            root: profile_dir.join("evolve").join("cache"),
        }
    }

    /// The key of a session.
    #[must_use]
    pub fn session_key(key: &SessionKey<'_>) -> String {
        let mut hash = Sha256::new();
        for part in [
            key.label,
            key.mission,
            key.system,
            key.model,
            PROMPT_VERSION,
        ] {
            field(&mut hash, part.as_bytes());
        }
        for relative in files_under(key.evidence) {
            field(&mut hash, relative.as_bytes());
            field(
                &mut hash,
                &std::fs::read(key.evidence.join(&relative)).unwrap_or_default(),
            );
        }
        hex(&hash.finalize())
    }

    fn session_dir(&self, key: &str) -> PathBuf {
        self.root.join("sessions").join(key)
    }

    /// The cached session of `key`, when present and untouched.
    #[must_use]
    pub fn session(&self, key: &str) -> Option<CachedSession> {
        let dir = self.session_dir(key);
        let meta: SessionMeta =
            serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).ok()?).ok()?;
        let workspace = dir.join("workspace");
        if meta.prompt_version != PROMPT_VERSION || dir_fingerprint(&workspace) != meta.workspace {
            tracing::info!(key, label = %meta.label, "cached improver session changed on disk; it runs again");
            return None;
        }
        let response = std::fs::read_to_string(dir.join("response.md")).ok()?;
        Some(CachedSession {
            response,
            workspace,
        })
    }

    /// Stores a session: its mission, reply, meta and the harness it left.
    ///
    /// # Errors
    ///
    /// I/O errors.
    pub fn put_session(
        &self,
        key: &str,
        mission: &str,
        response: &str,
        meta: &SessionMeta,
        harness: &Tree,
    ) -> std::io::Result<()> {
        self.publish(&self.session_dir(key), |staging| {
            harness.write(&staging.join("workspace"))?;
            let mut meta = meta.clone();
            meta.workspace = dir_fingerprint(&staging.join("workspace"));
            std::fs::write(staging.join("mission.md"), mission)?;
            std::fs::write(staging.join("response.md"), response)?;
            std::fs::write(
                staging.join("meta.json"),
                serde_json::to_vec_pretty(&meta).map_err(std::io::Error::other)?,
            )
        })
    }

    /// The key of a rollout.
    #[must_use]
    pub fn rollout_key(task: &str, harness: &str, model: &str, label: &str, seed: u64) -> String {
        let mut hash = Sha256::new();
        for part in [task, harness, model, label, &seed.to_string()] {
            field(&mut hash, part.as_bytes());
        }
        hex(&hash.finalize())
    }

    /// The directory of a cached rollout (`outcome.json`, `images/`).
    #[must_use]
    pub fn rollout_dir(&self, key: &str) -> PathBuf {
        self.root.join("rollouts").join(key)
    }

    /// The cached rollout of `key`.
    #[must_use]
    pub fn rollout(&self, key: &str) -> Option<RolloutOutcome> {
        let text = std::fs::read_to_string(self.rollout_dir(key).join("outcome.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Stores a rollout and its images.
    ///
    /// # Errors
    ///
    /// I/O errors.
    pub fn put_rollout(
        &self,
        key: &str,
        outcome: &RolloutOutcome,
        images: &Path,
    ) -> std::io::Result<()> {
        self.publish(&self.rollout_dir(key), |staging| {
            if images.is_dir() {
                private_fs::copy_tree(images, &staging.join("images"))?;
            }
            std::fs::write(
                staging.join("outcome.json"),
                serde_json::to_vec_pretty(outcome).map_err(std::io::Error::other)?,
            )
        })
    }

    /// Builds `target` under a staging name and renames it into place
    /// (replacing a stale entry).
    fn publish(
        &self,
        target: &Path,
        fill: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let parent = target.parent().unwrap_or(&self.root);
        private_fs::create_dir(parent)?;
        let staging = parent.join(format!(
            ".tmp-{}-{}",
            ulid::Ulid::generate(),
            std::process::id()
        ));
        private_fs::create_dir(&staging)?;
        let built = fill(&staging).and_then(|()| {
            if target.exists() {
                std::fs::remove_dir_all(target)?;
            }
            std::fs::rename(&staging, target)
        });
        if built.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        built
    }
}
