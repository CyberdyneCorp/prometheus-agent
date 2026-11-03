//! Inbound media (spec: platform-adapters, Inbound media limits and
//! caching): an attachment is downloaded through its adapter under
//! `gateway.media.max_inbound_bytes`, enforced against both the declared
//! length and the running byte count, into the owning profile's
//! `cache/media/` named by its content hash; nothing of an oversize
//! download is kept. Cached files older than
//! `gateway.media.cache_max_age_secs` are deleted unless a kept (pinned)
//! session mentions them.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use futures_util::StreamExt as _;
use pmt_config::schema::MediaConfig;
use sha2::{Digest as _, Sha256};

use super::adapter::{Adapter, Attachment, MediaKind, SendError};

/// Why an attachment was not cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Over the cap: `(seen, limit)` in bytes.
    TooLarge(u64, u64),
    /// The download failed.
    Failed(String),
    /// The adapter does not download media (its locations are usable as
    /// they are).
    Unsupported,
}

impl Refused {
    /// What the user is told.
    pub(crate) fn notice(&self) -> String {
        match self {
            Self::TooLarge(seen, limit) => {
                format!(
                    "Attachment too large ({} MB > {} MB)",
                    mb(*seen),
                    mb(*limit)
                )
            }
            Self::Failed(why) => format!("An attachment could not be downloaded: {why}"),
            Self::Unsupported => String::new(),
        }
    }
}

fn mb(bytes: u64) -> u64 {
    bytes.div_ceil(1024 * 1024)
}

/// The cache directory of a profile.
pub(crate) fn cache_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("cache").join("media")
}

fn over(limit: u64, n: u64) -> bool {
    limit > 0 && n > limit
}

/// Downloads `attachment` into `cache`, returning the cached path.
pub(crate) async fn fetch(
    adapter: &dyn Adapter,
    attachment: &Attachment,
    config: &MediaConfig,
    cache: &Path,
) -> Result<PathBuf, Refused> {
    let limit = config.max_inbound_bytes;
    let mut stream = adapter.open_media(attachment).await.map_err(|e| match e {
        SendError::Unsupported => Refused::Unsupported,
        other => Refused::Failed(other.describe()),
    })?;
    if let Some(declared) = stream.declared_len
        && over(limit, declared)
    {
        return Err(Refused::TooLarge(declared, limit));
    }
    std::fs::create_dir_all(cache).map_err(|e| Refused::Failed(e.to_string()))?;
    let partial = cache.join(format!(".partial-{}", crate::tickets::random_hex(8)));
    let outcome = write(&mut stream.chunks, &partial, limit).await;
    let digest = match outcome {
        Ok(digest) => digest,
        Err(refused) => {
            let _ = std::fs::remove_file(&partial);
            return Err(refused);
        }
    };
    let name = format!("{digest}{}", extension(attachment));
    let path = cache.join(name);
    std::fs::rename(&partial, &path).map_err(|e| Refused::Failed(e.to_string()))?;
    Ok(path)
}

/// Streams the body into `path`, hashing it; stops at the cap.
async fn write(
    chunks: &mut (impl futures_util::Stream<Item = Result<Vec<u8>, String>> + Unpin),
    path: &Path,
    limit: u64,
) -> Result<String, Refused> {
    let mut file = std::fs::File::create(path).map_err(|e| Refused::Failed(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut seen: u64 = 0;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(Refused::Failed)?;
        seen = seen.saturating_add(chunk.len() as u64);
        if over(limit, seen) {
            return Err(Refused::TooLarge(seen, limit));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .map_err(|e| Refused::Failed(e.to_string()))?;
    }
    Ok(crate::tickets::hex(&hasher.finalize()))
}

/// The cached file's extension: the platform's file name's, else the
/// kind's usual one.
fn extension(attachment: &Attachment) -> String {
    let named = attachment
        .name
        .as_deref()
        .and_then(|n| Path::new(n).extension())
        .map(|e| e.to_string_lossy().to_lowercase())
        .filter(|e| e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()));
    let ext = named.unwrap_or_else(|| {
        match attachment.kind {
            MediaKind::Image => "jpg",
            MediaKind::Voice => "ogg",
            MediaKind::Audio => "mp3",
            MediaKind::Video => "mp4",
            MediaKind::Document => "bin",
            MediaKind::Location | MediaKind::LiveLocation => "geo",
        }
        .to_owned()
    });
    format!(".{ext}")
}

/// Deletes cached files older than `max_age` at `now` that `kept` does
/// not claim; returns how many were deleted.
pub(crate) fn sweep(
    cache: &Path,
    now: SystemTime,
    max_age: Duration,
    kept: impl Fn(&str) -> bool,
) -> usize {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return 0;
    };
    let mut deleted = 0;
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age > max_age);
        let name = entry.file_name().to_string_lossy().into_owned();
        if old && !kept(&name) && std::fs::remove_file(entry.path()).is_ok() {
            deleted += 1;
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_files_go_unless_a_kept_session_mentions_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), "a").unwrap();
        std::fs::write(dir.path().join("b.jpg"), "b").unwrap();
        let hour = Duration::from_hours(1);
        let now = SystemTime::now();
        assert_eq!(sweep(dir.path(), now, hour, |_| false), 0);
        let later = now + 2 * hour;
        assert_eq!(sweep(dir.path(), later, hour, |name| name == "b.jpg"), 1);
        assert!(!dir.path().join("a.jpg").exists());
        assert!(dir.path().join("b.jpg").exists());
    }

    #[test]
    fn the_notice_states_both_sizes_in_megabytes() {
        let notice = Refused::TooLarge(200 * 1024 * 1024, 128 * 1024 * 1024).notice();
        assert_eq!(notice, "Attachment too large (200 MB > 128 MB)");
    }
}
