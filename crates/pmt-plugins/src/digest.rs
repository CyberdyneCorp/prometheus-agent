//! A plugin's identity digest: SHA-256 over every file of its directory
//! (relative path and contents, in path order), so any change to the
//! manifest, the component or a bundled skill yields a new identity and
//! the grants of the old one stop applying (spec: plugin-system, Capability
//! grants are user-approved and digest-bound; security-boundaries, Grants
//! pinned to plugin identity).

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use sha2::{Digest as _, Sha256};

/// The install record, written after the digest is computed: not part of it.
pub const INSTALL_RECORD: &str = ".install.toml";

/// `sha256:<64 hex>` of the directory.
///
/// # Errors
///
/// `plugins.discovery` when a file cannot be read or the directory holds a
/// symbolic link (a plugin cannot point outside itself).
pub fn of_dir(dir: &Path) -> Result<String, Error> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for relative in &files {
        let path = dir.join(relative);
        let bytes =
            std::fs::read(&path).map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &path, &e))?;
        let name = relative.to_string_lossy().replace('\\', "/");
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("sha256:{}", hex(&hasher.finalize())))
}

/// `sha256:<64 hex>` of bytes (an embedded plugin).
#[must_use]
pub fn of_bytes(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    format!("sha256:{}", hex(&hasher.finalize()))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, dir, &e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, dir, &e))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &path, &e))?;
        if kind.is_symlink() {
            return Err(Error::new(
                &codes::PLUGINS_DISCOVERY,
                format!(
                    "{} is a symbolic link; plugin directories may not hold links",
                    path.display()
                ),
            ));
        }
        if kind.is_dir() {
            collect(root, &path, out)?;
        } else if dir != root || entry.file_name() != INSTALL_RECORD {
            out.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_change_moves_the_digest_but_the_install_record_does_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.toml"), "a").unwrap();
        std::fs::create_dir(dir.path().join("skills")).unwrap();
        std::fs::write(dir.path().join("skills/x.md"), "b").unwrap();
        let first = of_dir(dir.path()).unwrap();
        assert!(first.starts_with("sha256:") && first.len() == 71);
        std::fs::write(dir.path().join(INSTALL_RECORD), "source = 'x'").unwrap();
        assert_eq!(of_dir(dir.path()).unwrap(), first);
        std::fs::write(dir.path().join("skills/x.md"), "c").unwrap();
        assert_ne!(of_dir(dir.path()).unwrap(), first);
    }
}
