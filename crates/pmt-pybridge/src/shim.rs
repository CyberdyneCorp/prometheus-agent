//! The Python side of the bridge, compiled into the binary (spec:
//! python-bridge, Hermes import shims): the sidecar loop
//! (`prometheus_bridge`), the documented Hermes API modules (`hermes_api`)
//! and the entry script. Each profile gets a copy under
//! `<profile>/python/shim/<digest12>/`, written once per content digest and
//! read-only to sidecars.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use sha2::{Digest as _, Sha256};

/// The entry script, relative to the shim directory.
pub const ENTRY: &str = "bridge_main.py";

/// Every shim file: path relative to the shim directory, and content.
pub const FILES: &[(&str, &str)] = &[
    ("bridge_main.py", include_str!("../python/bridge_main.py")),
    (
        "hermes_api/agent/__init__.py",
        include_str!("../python/hermes_api/agent/__init__.py"),
    ),
    (
        "hermes_api/agent/account_usage.py",
        include_str!("../python/hermes_api/agent/account_usage.py"),
    ),
    (
        "hermes_api/agent/context_engine.py",
        include_str!("../python/hermes_api/agent/context_engine.py"),
    ),
    (
        "hermes_api/agent/context_references.py",
        include_str!("../python/hermes_api/agent/context_references.py"),
    ),
    (
        "hermes_api/agent/image_gen_provider.py",
        include_str!("../python/hermes_api/agent/image_gen_provider.py"),
    ),
    (
        "hermes_api/agent/memory_provider.py",
        include_str!("../python/hermes_api/agent/memory_provider.py"),
    ),
    (
        "hermes_api/agent/provider_base.py",
        include_str!("../python/hermes_api/agent/provider_base.py"),
    ),
    (
        "hermes_api/agent/secret_scope.py",
        include_str!("../python/hermes_api/agent/secret_scope.py"),
    ),
    (
        "hermes_api/agent/secret_sources/__init__.py",
        include_str!("../python/hermes_api/agent/secret_sources/__init__.py"),
    ),
    (
        "hermes_api/agent/secret_sources/base.py",
        include_str!("../python/hermes_api/agent/secret_sources/base.py"),
    ),
    (
        "hermes_api/agent/transcription_provider.py",
        include_str!("../python/hermes_api/agent/transcription_provider.py"),
    ),
    (
        "hermes_api/agent/tts_provider.py",
        include_str!("../python/hermes_api/agent/tts_provider.py"),
    ),
    (
        "hermes_api/agent/video_gen_provider.py",
        include_str!("../python/hermes_api/agent/video_gen_provider.py"),
    ),
    (
        "hermes_api/agent/web_search_provider.py",
        include_str!("../python/hermes_api/agent/web_search_provider.py"),
    ),
    (
        "hermes_api/hermes_cli/__init__.py",
        include_str!("../python/hermes_api/hermes_cli/__init__.py"),
    ),
    (
        "hermes_api/hermes_cli/version_info.py",
        include_str!("../python/hermes_api/hermes_cli/version_info.py"),
    ),
    (
        "hermes_api/providers/__init__.py",
        include_str!("../python/hermes_api/providers/__init__.py"),
    ),
    (
        "hermes_api/providers/base.py",
        include_str!("../python/hermes_api/providers/base.py"),
    ),
    (
        "prometheus_bridge/__init__.py",
        include_str!("../python/prometheus_bridge/__init__.py"),
    ),
    (
        "prometheus_bridge/cli.py",
        include_str!("../python/prometheus_bridge/cli.py"),
    ),
    (
        "prometheus_bridge/context.py",
        include_str!("../python/prometheus_bridge/context.py"),
    ),
    (
        "prometheus_bridge/hooks.py",
        include_str!("../python/prometheus_bridge/hooks.py"),
    ),
    (
        "prometheus_bridge/imports.py",
        include_str!("../python/prometheus_bridge/imports.py"),
    ),
    (
        "prometheus_bridge/memory.py",
        include_str!("../python/prometheus_bridge/memory.py"),
    ),
    (
        "prometheus_bridge/sidecar.py",
        include_str!("../python/prometheus_bridge/sidecar.py"),
    ),
    (
        "prometheus_bridge/state.py",
        include_str!("../python/prometheus_bridge/state.py"),
    ),
    (
        "prometheus_bridge/typed.py",
        include_str!("../python/prometheus_bridge/typed.py"),
    ),
    (
        "prometheus_bridge/wire.py",
        include_str!("../python/prometheus_bridge/wire.py"),
    ),
];

/// Lower-case hex SHA-256 over every file's path and content.
#[must_use]
pub fn digest() -> String {
    let mut hasher = Sha256::new();
    for (path, text) in FILES {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(text.as_bytes());
        hasher.update([0]);
    }
    crate::hex(&hasher.finalize())
}

/// Writes the shim under `root` (once per digest) and returns its
/// directory.
///
/// # Errors
///
/// `bridge.environment` when the files cannot be written.
pub fn materialize(root: &Path) -> Result<PathBuf, Error> {
    let dir = root.join(&digest()[..12]);
    if dir.join(ENTRY).is_file() {
        return Ok(dir);
    }
    let staging = root.join(format!(".staging-{}", std::process::id()));
    let io = |path: &Path, e: std::io::Error| Error::io(&codes::BRIDGE_ENVIRONMENT, path, &e);
    let _ = std::fs::remove_dir_all(&staging);
    for (path, text) in FILES {
        let target = staging.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        std::fs::write(&target, text).map_err(|e| io(&target, e))?;
    }
    match std::fs::rename(&staging, &dir) {
        Ok(()) => Ok(dir),
        // Another process wrote the same digest first.
        Err(_) if dir.join(ENTRY).is_file() => {
            let _ = std::fs::remove_dir_all(&staging);
            Ok(dir)
        }
        Err(e) => Err(io(&dir, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_python_file_of_the_crate_is_embedded() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
        let mut on_disk = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "py") {
                    on_disk.push(
                        path.strip_prefix(&root)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        on_disk.sort();
        let embedded: Vec<&str> = FILES.iter().map(|(p, _)| *p).collect();
        assert_eq!(on_disk, embedded);
    }

    #[test]
    fn the_shim_is_written_once_per_digest() {
        let root = tempfile::tempdir().unwrap();
        let dir = materialize(root.path()).unwrap();
        assert!(dir.join(ENTRY).is_file());
        assert_eq!(materialize(root.path()).unwrap(), dir);
    }
}
