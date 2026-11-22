//! First-party plugins compiled into the binary (spec: plugin-system,
//! Discovery sources and identity): their identity digest, and the copy of
//! a `python` plugin's source files the bridge runs.
//!
//! A WASM component runs from its bytes. A python plugin needs a directory,
//! so its files are written to `<profile>/python/embedded/<id>/<digest12>/`
//! once per digest (a new binary with changed files gets a new directory)
//! and never edited in place.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};

use crate::digest;
use crate::discovery::EmbeddedPlugin;

/// The identity digest of `plugin`: its manifest, component and files.
#[must_use]
pub fn digest(plugin: &EmbeddedPlugin) -> String {
    let mut parts: Vec<&[u8]> = vec![plugin.manifest.as_bytes(), plugin.component];
    for (path, text) in plugin.files {
        parts.push(path.as_bytes());
        parts.push(text.as_bytes());
    }
    digest::of_bytes(&parts)
}

/// Where the files of embedded plugin `id` with `digest` are written.
#[must_use]
pub fn dir(profile_dir: &Path, id: &str, digest: &str) -> PathBuf {
    let hex = digest.trim_start_matches("sha256:");
    profile_dir
        .join("python")
        .join("embedded")
        .join(id)
        .join(&hex[..hex.len().min(12)])
}

/// The marker written last: a directory without it is incomplete.
const COMPLETE: &str = ".complete";

/// Writes `plugin`'s files to `dir` unless they are already there.
///
/// # Errors
///
/// `plugins.load` when the files cannot be written.
pub fn materialize(plugin: &EmbeddedPlugin, dir: &Path) -> Result<(), Error> {
    if dir.join(COMPLETE).is_file() {
        return Ok(());
    }
    let io = |path: &Path, e: std::io::Error| Error::io(&codes::PLUGINS_LOAD, path, &e);
    let parent = dir.parent().unwrap_or(dir);
    let staging = parent.join(format!(".staging-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    for (path, text) in plugin.files {
        let target = staging.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        std::fs::write(&target, text).map_err(|e| io(&target, e))?;
    }
    std::fs::write(staging.join(COMPLETE), "").map_err(|e| io(&staging, e))?;
    match std::fs::rename(&staging, dir) {
        Ok(()) => Ok(()),
        // Another process wrote the same digest first.
        Err(_) if dir.join(COMPLETE).is_file() => {
            let _ = std::fs::remove_dir_all(&staging);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(io(dir, e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLUGIN: EmbeddedPlugin = EmbeddedPlugin {
        manifest: "id = \"x\"",
        component: b"",
        files: &[("__init__.py", "X = 1\n"), ("sub/mod.py", "Y = 2\n")],
        enabled_by_default: false,
    };

    #[test]
    fn files_are_written_once_under_their_digest() {
        let profile = tempfile::tempdir().unwrap();
        let dir = dir(profile.path(), "x", &digest(&PLUGIN));
        materialize(&PLUGIN, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/mod.py")).unwrap(),
            "Y = 2\n"
        );
        materialize(&PLUGIN, &dir).unwrap();
        let entries = std::fs::read_dir(dir.parent().unwrap()).unwrap().count();
        assert_eq!(entries, 1, "no staging directory is left behind");
    }

    #[test]
    fn the_digest_covers_the_files() {
        let other = EmbeddedPlugin {
            files: &[("__init__.py", "X = 2\n")],
            ..PLUGIN
        };
        assert_ne!(digest(&PLUGIN), digest(&other));
        let component = EmbeddedPlugin {
            files: &[],
            ..PLUGIN
        };
        assert_eq!(
            digest(&component),
            digest::of_bytes(&[PLUGIN.manifest.as_bytes(), b""])
        );
    }
}
