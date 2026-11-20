//! The library on disk (spec: experience-library, Library layout; Index;
//! Exposure to proposers; Retention cap; Library CLI).
//!
//! ```text
//! <profile>/experience/<domain>/INDEX.md
//! <profile>/experience/<domain>/entries/<entry_id>/{EXPERIENCE.md, entry.toml, harness/}
//! <profile>/experience/events.jsonl      library events (no session owns them)
//! ```
//!
//! Every change runs under `<profile>/experience/library.lock` and ends
//! with the domain's `INDEX.md` rebuilt. Entries are written staged and
//! renamed into place, so a reader never sees half an entry.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::harness::Tree;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde_json::json;

use super::entry::{EntryMeta, Status};
use super::log::LibraryLog;

/// The empty marker of `INDEX.md`.
pub const EMPTY: &str = "_(empty — no adopted lessons in this domain yet)_";

/// The guidance at the top of every `INDEX.md`.
pub const GUIDANCE: &str = "Each entry is a harness adopted for an earlier task in this \
domain, with the lesson drawn from it. Entries are examples to copy and re-verify against \
this task's trajectories, not rules: an adoption alone never proves an edit helped.";

fn io(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::EXPERIENCE_IO, path, err)
}

/// `no experience entry '<id>'` (exit 1).
#[must_use]
pub fn not_found(id: &str) -> Error {
    Error::new(
        &codes::EXPERIENCE_ENTRY_NOT_FOUND,
        format!("no experience entry '{id}'"),
    )
}

/// One entry ready to be written.
#[derive(Debug, Clone)]
pub struct NewEntry {
    /// `entry.toml` (`entry_id` and `domain` decide the directory).
    pub meta: EntryMeta,
    /// `EXPERIENCE.md`.
    pub document: String,
    /// `harness/`.
    pub tree: Tree,
}

/// What writing an entry did.
#[derive(Debug, Clone)]
pub struct Written {
    /// The entry.
    pub meta: EntryMeta,
    /// Entries evicted by the retention cap.
    pub evicted: Vec<String>,
}

/// `<profile>/experience`.
#[derive(Debug, Clone)]
pub struct Library {
    root: PathBuf,
}

impl Library {
    /// The library of the profile at `profile_dir`.
    #[must_use]
    pub fn of(profile_dir: &Path) -> Self {
        Self {
            root: profile_dir.join("experience"),
        }
    }

    /// `<profile>/experience`.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<profile>/experience/<domain>`.
    #[must_use]
    pub fn domain_dir(&self, domain: &str) -> PathBuf {
        self.root.join(domain)
    }

    /// The directory of `entry_id` in `domain`.
    #[must_use]
    pub fn entry_dir(&self, domain: &str, entry_id: &str) -> PathBuf {
        self.domain_dir(domain).join("entries").join(entry_id)
    }

    /// The library's event log.
    #[must_use]
    pub fn log(&self) -> LibraryLog {
        LibraryLog::new(&self.root)
    }

    /// Takes the library lock.
    ///
    /// # Errors
    ///
    /// `experience.io`.
    pub fn lock(&self) -> Result<std::fs::File, Error> {
        private_fs::create_dir(&self.root).map_err(|e| io(&self.root, &e))?;
        let path = self.root.join("library");
        private_fs::lock_beside(&path).map_err(|e| io(&path, &e))
    }

    /// The domains with a directory, sorted.
    #[must_use]
    pub fn domains(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| super::domain::is_valid(n))
            .collect();
        names.sort();
        names
    }

    /// The entries of `domain`, newest first.
    #[must_use]
    pub fn entries(&self, domain: &str) -> Vec<EntryMeta> {
        let dir = self.domain_dir(domain).join("entries");
        let Ok(read) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut found: Vec<EntryMeta> = read
            .filter_map(Result::ok)
            .filter_map(|e| read_meta(&e.path().join("entry.toml")))
            .collect();
        found.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.entry_id.cmp(&a.entry_id))
        });
        found
    }

    /// Every entry of every domain (domains sorted, entries newest first).
    #[must_use]
    pub fn all(&self) -> Vec<EntryMeta> {
        self.domains()
            .iter()
            .flat_map(|d| self.entries(d))
            .collect()
    }

    /// The visible entries of `domain`, pinned first, then newest.
    #[must_use]
    pub fn visible(&self, domain: &str) -> Vec<EntryMeta> {
        let mut shown: Vec<EntryMeta> = self
            .entries(domain)
            .into_iter()
            .filter(EntryMeta::visible)
            .collect();
        shown.sort_by_key(|e| !e.pinned);
        shown
    }

    /// Entry `id`, in whichever domain holds it.
    ///
    /// # Errors
    ///
    /// `experience.entry_not_found` (1).
    pub fn find(&self, id: &str) -> Result<EntryMeta, Error> {
        let plain = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !plain {
            return Err(not_found(id));
        }
        self.domains()
            .iter()
            .find_map(|d| read_meta(&self.entry_dir(d, id).join("entry.toml")))
            .ok_or_else(|| not_found(id))
    }

    /// `EXPERIENCE.md` of `meta`.
    #[must_use]
    pub fn document(&self, meta: &EntryMeta) -> String {
        std::fs::read_to_string(
            self.entry_dir(&meta.domain, &meta.entry_id)
                .join("EXPERIENCE.md"),
        )
        .unwrap_or_default()
    }

    /// The harness tree of `meta`.
    ///
    /// # Errors
    ///
    /// `experience.io`.
    pub fn tree(&self, meta: &EntryMeta) -> Result<Tree, Error> {
        let dir = self.entry_dir(&meta.domain, &meta.entry_id).join("harness");
        Tree::read(&dir).map(|(t, _)| t).map_err(|e| io(&dir, &e))
    }

    /// Writes `meta` back (under the caller's lock).
    ///
    /// # Errors
    ///
    /// `experience.io`.
    pub fn save(&self, meta: &EntryMeta) -> Result<(), Error> {
        let path = self
            .entry_dir(&meta.domain, &meta.entry_id)
            .join("entry.toml");
        let text =
            toml::to_string(meta).map_err(|e| Error::new(&codes::EXPERIENCE_IO, e.to_string()))?;
        private_fs::write_atomic(&path, text.as_bytes()).map_err(|e| io(&path, &e))
    }

    /// Makes sure `domain` has its directory and `INDEX.md`.
    ///
    /// # Errors
    ///
    /// `experience.io`.
    pub fn ensure(&self, domain: &str) -> Result<(), Error> {
        let index = self.domain_dir(domain).join("INDEX.md");
        if index.is_file() {
            return Ok(());
        }
        let _lock = self.lock()?;
        self.rebuild_index(domain)
    }

    /// Rewrites `INDEX.md` of `domain` from its visible entries.
    ///
    /// # Errors
    ///
    /// `experience.io`.
    pub fn rebuild_index(&self, domain: &str) -> Result<(), Error> {
        let dir = self.domain_dir(domain);
        private_fs::create_dir(&dir).map_err(|e| io(&dir, &e))?;
        let path = dir.join("INDEX.md");
        let text = index_text(domain, &self.visible_newest(domain));
        private_fs::write_atomic(&path, text.as_bytes()).map_err(|e| io(&path, &e))
    }

    fn visible_newest(&self, domain: &str) -> Vec<EntryMeta> {
        self.entries(domain)
            .into_iter()
            .filter(EntryMeta::visible)
            .collect()
    }

    /// Writes `new`, evicting the oldest unpinned entries beyond `cap`.
    ///
    /// # Errors
    ///
    /// `experience.io`.
    pub fn write(&self, new: &NewEntry, cap: usize) -> Result<Written, Error> {
        let _lock = self.lock()?;
        let meta = &new.meta;
        let dir = self.entry_dir(&meta.domain, &meta.entry_id);
        let parent = dir.parent().unwrap_or(&self.root).to_path_buf();
        private_fs::create_dir(&parent).map_err(|e| io(&parent, &e))?;
        let staged = parent.join(format!(".{}.tmp", meta.entry_id));
        let _ = std::fs::remove_dir_all(&staged);
        private_fs::create_dir(&staged).map_err(|e| io(&staged, &e))?;
        new.tree
            .write(&staged.join("harness"))
            .map_err(|e| io(&staged, &e))?;
        std::fs::write(staged.join("EXPERIENCE.md"), &new.document).map_err(|e| io(&staged, &e))?;
        let text =
            toml::to_string(meta).map_err(|e| Error::new(&codes::EXPERIENCE_IO, e.to_string()))?;
        std::fs::write(staged.join("entry.toml"), text).map_err(|e| io(&staged, &e))?;
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| io(&dir, &e))?;
        }
        std::fs::rename(&staged, &dir).map_err(|e| io(&dir, &e))?;
        self.log().entry("experience.entry.added", meta);
        let evicted = self.evict(&meta.domain, cap)?;
        self.rebuild_index(&meta.domain)?;
        Ok(Written {
            meta: meta.clone(),
            evicted,
        })
    }

    /// Removes the oldest unpinned entries while `domain` holds more than
    /// `cap`.
    fn evict(&self, domain: &str, cap: usize) -> Result<Vec<String>, Error> {
        let mut entries = self.entries(domain);
        let mut evicted = Vec::new();
        while entries.len() > cap {
            let Some(oldest) = entries.iter().rposition(|e| !e.pinned) else {
                break;
            };
            let gone = entries.remove(oldest);
            let dir = self.entry_dir(domain, &gone.entry_id);
            std::fs::remove_dir_all(&dir).map_err(|e| io(&dir, &e))?;
            tracing::info!(entry = %gone.entry_id, domain, "experience entry evicted");
            self.log().entry("experience.entry.evicted", &gone);
            evicted.push(gone.entry_id);
        }
        Ok(evicted)
    }

    /// Removes entry `id` (its harness version stays).
    ///
    /// # Errors
    ///
    /// `experience.entry_not_found`, `experience.io`.
    pub fn forget(&self, id: &str) -> Result<EntryMeta, Error> {
        let _lock = self.lock()?;
        let meta = self.find(id)?;
        let dir = self.entry_dir(&meta.domain, id);
        std::fs::remove_dir_all(&dir).map_err(|e| io(&dir, &e))?;
        self.rebuild_index(&meta.domain)?;
        Ok(meta)
    }

    /// Applies `edit` to entry `id` and rebuilds its index; the entry
    /// after the edit and whether its status changed.
    ///
    /// # Errors
    ///
    /// `experience.entry_not_found`, `experience.io`.
    pub fn update(&self, id: &str, edit: impl FnOnce(&mut EntryMeta)) -> Result<EntryMeta, Error> {
        let _lock = self.lock()?;
        let mut meta = self.find(id)?;
        let before = meta.status;
        edit(&mut meta);
        self.save(&meta)?;
        if meta.status != before {
            self.log().entry("experience.entry.status_changed", &meta);
        }
        self.rebuild_index(&meta.domain)?;
        Ok(meta)
    }

    /// Sets the status of entry `id`.
    ///
    /// # Errors
    ///
    /// As [`Library::update`].
    pub fn set_status(&self, id: &str, status: Status) -> Result<EntryMeta, Error> {
        self.update(id, |m| m.status = status)
    }

    /// Copies `INDEX.md` and the newest `max` visible entries of `domain`
    /// (pinned first) into `dest`; whether any entry was visible (else
    /// `dest` is not created).
    ///
    /// # Errors
    ///
    /// `experience.io`.
    pub fn snapshot(&self, domain: &str, dest: &Path, max: usize) -> Result<bool, Error> {
        let shown: Vec<EntryMeta> = self.visible(domain).into_iter().take(max).collect();
        if shown.is_empty() {
            return Ok(false);
        }
        private_fs::create_dir(dest).map_err(|e| io(dest, &e))?;
        let index = index_text(domain, &shown);
        std::fs::write(dest.join("INDEX.md"), index).map_err(|e| io(dest, &e))?;
        for meta in &shown {
            let from = self.entry_dir(domain, &meta.entry_id);
            let to = dest.join("entries").join(&meta.entry_id);
            private_fs::copy_tree(&from, &to).map_err(|e| io(&from, &e))?;
        }
        Ok(true)
    }
}

fn read_meta(path: &Path) -> Option<EntryMeta> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text)
        .inspect_err(|e| tracing::warn!(path = %path.display(), "unreadable entry.toml: {e}"))
        .ok()
}

/// The text of `INDEX.md` for `entries` (already filtered and ordered).
#[must_use]
pub fn index_text(domain: &str, entries: &[EntryMeta]) -> String {
    let mut text = format!("# Experience library — {domain}\n\n{GUIDANCE}\n\n");
    if entries.is_empty() {
        text.push_str(EMPTY);
        text.push('\n');
    }
    for e in entries {
        let _ = write!(
            text,
            "## entries/{}/\n\n- components: {}\n- confidence: {}\n- cue: {}\n\n",
            e.entry_id,
            e.inventory,
            e.confidence.as_str(),
            e.cue
        );
    }
    text
}

impl LibraryLog {
    /// An entry event (`experience.entry.*`).
    pub fn entry(&self, kind: &str, meta: &EntryMeta) {
        self.emit(
            kind,
            &json!({"entry_id": meta.entry_id, "status": meta.status.as_str()}),
        );
    }
}
