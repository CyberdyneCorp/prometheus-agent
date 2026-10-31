//! The on-disk store of extracted full texts (spec: web-and-media-tools,
//! Extracted full text is stored and capped).
//!
//! A truncated extraction keeps its clean full text under
//! `<profile>/cache/web/` (0600 files in a 0700 directory) so the model can
//! page through the omitted middle with `read_file`. One file per (URL,
//! format, backend); texts are capped and expire after a week.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pmt_core::private_fs;
use sha2::{Digest as _, Sha256};

/// Most characters of one stored text.
pub const MAX_CHARS: usize = 2_000_000;
/// Stored texts older than this are deleted.
pub const MAX_AGE: Duration = Duration::from_hours(7 * 24);
/// How often a long-running process prunes.
pub const PRUNE_EVERY: Duration = Duration::from_hours(1);

/// The full-text store of one profile.
#[derive(Debug, Clone)]
pub struct FullTextStore {
    dir: PathBuf,
}

impl FullTextStore {
    /// The store of the profile whose home is `profile_dir`.
    #[must_use]
    pub fn for_profile(profile_dir: &Path) -> Self {
        Self::at(profile_dir.join("cache").join("web"))
    }

    /// A store in `dir`.
    #[must_use]
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The directory holding the stored texts.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the text of (`url`, `format`, `backend`) is stored.
    #[must_use]
    pub fn path_of(&self, url: &str, format: &str, backend: &str) -> PathBuf {
        let mut hash = Sha256::new();
        for part in [url, format, backend] {
            hash.update(part.as_bytes());
            hash.update([0]);
        }
        let digest = hash.finalize();
        let name = digest.iter().fold(String::new(), |mut name, byte| {
            let _ = write!(name, "{byte:02x}");
            name
        });
        self.dir.join(format!("{name}.md"))
    }

    /// Stores `text`, capped at [`MAX_CHARS`] with a final line stating the
    /// cap, and returns its path.
    ///
    /// # Errors
    ///
    /// The directory or file could not be written.
    pub fn save(&self, url: &str, format: &str, backend: &str, text: &str) -> io::Result<PathBuf> {
        private_fs::create_dir(&self.dir)?;
        let path = self.path_of(url, format, backend);
        private_fs::write_atomic(&path, capped(text).as_bytes())?;
        Ok(path)
    }

    /// Deletes stored texts last written more than `max_age` ago; returns
    /// how many were removed. A missing store is empty.
    ///
    /// # Errors
    ///
    /// The directory could not be listed.
    pub fn prune(&self, max_age: Duration) -> io::Result<usize> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(err),
        };
        let now = SystemTime::now();
        let mut removed = 0;
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|at| now.duration_since(at).unwrap_or_default() > max_age);
            if old && fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Prunes now and every [`PRUNE_EVERY`] until `cancel` fires; the daemon
    /// runs it for each profile it serves.
    pub async fn prune_periodically(&self, cancel: &pmt_core::cancel::CancelToken) {
        let mut ticks = tokio::time::interval(PRUNE_EVERY);
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                _ = ticks.tick() => {
                    if let Err(err) = self.prune(MAX_AGE) {
                        tracing::warn!(dir = %self.dir.display(), %err, "could not prune stored web texts");
                    }
                }
            }
        }
    }
}

/// `text` within [`MAX_CHARS`], with the cap line when cut.
fn capped(text: &str) -> String {
    match text.char_indices().nth(MAX_CHARS) {
        None => text.to_owned(),
        Some((at, _)) => format!(
            "{}\n[stored text capped at {MAX_CHARS} characters]\n",
            &text[..at]
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{FullTextStore, MAX_CHARS};

    #[test]
    fn a_huge_page_is_capped_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = FullTextStore::for_profile(dir.path());
        let text = "y".repeat(5_000_000);
        let path = store
            .save("https://a.test/", "markdown", "firecrawl", &text)
            .unwrap();
        let stored = std::fs::read_to_string(&path).unwrap();
        let (body, cap) = stored.split_at(MAX_CHARS);
        assert!(body.chars().all(|c| c == 'y'));
        assert_eq!(cap, "\n[stored text capped at 2000000 characters]\n");
        assert!(path.starts_with(dir.path().join("cache/web")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn keys_are_url_format_and_backend_and_old_texts_expire() {
        let dir = tempfile::tempdir().unwrap();
        let store = FullTextStore::for_profile(dir.path());
        let a = store.path_of("https://a.test/", "markdown", "firecrawl");
        assert_eq!(a, store.path_of("https://a.test/", "markdown", "firecrawl"));
        assert_ne!(a, store.path_of("https://a.test/", "markdown", "tavily"));
        assert_ne!(a, store.path_of("https://b.test/", "markdown", "firecrawl"));
        store
            .save("https://a.test/", "markdown", "firecrawl", "x")
            .unwrap();
        assert_eq!(store.prune(Duration::from_hours(1)).unwrap(), 0);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(store.prune(Duration::from_millis(10)).unwrap(), 1);
        assert!(!a.exists());
    }
}
