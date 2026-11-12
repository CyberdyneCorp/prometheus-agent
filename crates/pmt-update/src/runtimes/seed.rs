//! The `-full` image's runtimes (spec: distribution-and-update, Container
//! image run modes): the image build installs Node, CPython, uv and
//! Chromium (downloaded and verified like any install) into a seed store
//! at [`IMAGE_SEED`]; on first start the daemon copies every version the
//! home's store lacks into `/data/runtimes/`, where the normal store finds
//! and verifies them. A version already there is left alone.

use std::path::Path;

/// Where the image keeps its seed store (`<seed>/runtimes/<name>/<dir>`).
pub const IMAGE_SEED: &str = "/opt/prometheus/seed";

/// Copies each `<name>/<version dir>` of `seed`'s runtime store missing
/// from `store_root`; returns the copied `<name>/<dir>` entries. Each one
/// becomes visible by a rename, so a crash never leaves half a runtime.
///
/// # Errors
///
/// The first I/O error.
pub fn copy_missing(seed: &Path, store_root: &Path) -> std::io::Result<Vec<String>> {
    let mut copied = Vec::new();
    let Ok(names) = std::fs::read_dir(seed.join("runtimes")) else {
        return Ok(copied);
    };
    for name in names.flatten().filter(|e| e.path().is_dir()) {
        for version in std::fs::read_dir(name.path())?
            .flatten()
            .filter(|e| e.path().is_dir())
        {
            let target = store_root.join(name.file_name()).join(version.file_name());
            if target.exists() {
                continue;
            }
            let staging = store_root
                .join(".staging")
                .join(format!("seed-{}", ulid::Ulid::generate()));
            copy_tree(&version.path(), &staging)?;
            std::fs::create_dir_all(target.parent().unwrap_or(store_root))?;
            std::fs::rename(&staging, &target)?;
            copied.push(format!(
                "{}/{}",
                name.file_name().to_string_lossy(),
                version.file_name().to_string_lossy()
            ));
        }
    }
    Ok(copied)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            copy_link(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_link(from: &Path, to: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(from)?, to)
}

#[cfg(not(unix))]
fn copy_link(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to).map(drop)
}

#[cfg(test)]
mod tests {
    use super::copy_missing;

    #[test]
    fn missing_versions_are_copied_and_present_ones_kept() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        let node = seed.join("runtimes/node/24.0.0-abc");
        std::fs::create_dir_all(node.join("bin")).unwrap();
        std::fs::write(node.join("bin/node"), "node").unwrap();
        let kept = seed.join("runtimes/uv/0.1-def");
        std::fs::create_dir_all(&kept).unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir_all(store.join("uv/0.1-def")).unwrap();
        std::fs::write(store.join("uv/0.1-def/mine"), "x").unwrap();
        let copied = copy_missing(&seed, &store).unwrap();
        assert_eq!(copied, ["node/24.0.0-abc"]);
        assert_eq!(
            std::fs::read_to_string(store.join("node/24.0.0-abc/bin/node")).unwrap(),
            "node"
        );
        assert!(store.join("uv/0.1-def/mine").exists());
        assert!(copy_missing(&seed, &store).unwrap().is_empty());
    }
}
