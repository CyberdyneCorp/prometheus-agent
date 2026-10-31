//! Private copies of `config.toml` under `<profile>/backups/config/`: one
//! before every write (the newest [`KEEP`] are kept) and one before every
//! schema migration (`config-v<old>-…`, never pruned).

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;

/// Write backups kept per profile.
pub(crate) const KEEP: usize = 10;

/// Prefix of write backups (`config-<timestamp>.toml`).
const WRITE_PREFIX: &str = "config-";
/// Prefix of migration backups, which pruning leaves alone.
const MIGRATION_PREFIX: &str = "config-v";

/// Writes `text` to `<dir>/<stem>-<UTC timestamp>.toml` (0600, never
/// overwriting) and returns the path.
pub(crate) fn save(dir: &Path, stem: &str, text: &str) -> Result<PathBuf, Error> {
    let io_error = |path: &Path, e: io::Error| {
        Error::new(
            &codes::HOME_IO,
            format!("cannot write backup {}: {e}", path.display()),
        )
    };
    private_fs::create_dir(dir).map_err(|e| io_error(dir, e))?;
    let stamp = jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%S%.3fZ")
        .to_string();
    for attempt in 0u32.. {
        let name = match attempt {
            0 => format!("{stem}-{stamp}.toml"),
            // `_` sorts after `.`, so names stay in creation order.
            n => format!("{stem}-{stamp}_{n:03}.toml"),
        };
        let path = dir.join(name);
        match private_fs::new_file_options().open(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())
                    .and_then(|()| file.sync_all())
                    .map_err(|e| io_error(&path, e))?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_error(&path, e)),
        }
    }
    unreachable!("the attempt counter is unbounded")
}

/// The write backups in `dir`, oldest first.
pub(crate) fn write_backups(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with(WRITE_PREFIX)
                    && !n.starts_with(MIGRATION_PREFIX)
                    && Path::new(n)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
            })
        })
        .collect();
    found.sort();
    found
}

/// Deletes the oldest write backups beyond [`KEEP`]. Failures only WARN:
/// an extra backup is harmless.
pub(crate) fn prune(dir: &Path) {
    let found = write_backups(dir);
    let excess = found.len().saturating_sub(KEEP);
    for old in &found[..excess] {
        if let Err(e) = std::fs::remove_file(old) {
            tracing::warn!("cannot remove old config backup {}: {e}", old.display());
        }
    }
}
