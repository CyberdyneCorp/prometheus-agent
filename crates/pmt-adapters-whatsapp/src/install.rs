//! The managed `WhatsApp` bridge runtime (spec: platform-adapters, `WhatsApp`
//! pairing and modes; First-party adapter transports and required
//! settings). The bridge (a Baileys-based Node service, adapted from the
//! MIT-licensed Hermes Agent bridge) ships inside this crate with its npm
//! lockfile; installing it writes those files to
//! `<home>/runtimes/whatsapp-bridge/` and runs `npm ci` with the managed
//! Node runtime, so every dependency is the lockfile's integrity-checked
//! version. A marker holding the files' digest makes a later install a
//! no-op until the shipped bridge changes.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use sha2::{Digest as _, Sha256};

/// The shipped bridge files, by name.
const FILES: &[(&str, &str)] = &[
    ("bridge.js", include_str!("../bridge/bridge.js")),
    (
        "bridge_helpers.js",
        include_str!("../bridge/bridge_helpers.js"),
    ),
    ("allowlist.js", include_str!("../bridge/allowlist.js")),
    ("outbound_ids.js", include_str!("../bridge/outbound_ids.js")),
    (
        "owner_message_gate.js",
        include_str!("../bridge/owner_message_gate.js"),
    ),
    ("package.json", include_str!("../bridge/package.json")),
    (
        "package-lock.json",
        include_str!("../bridge/package-lock.json"),
    ),
    ("LICENSE", include_str!("../bridge/LICENSE")),
];

/// The marker naming the installed bridge's digest.
const MARKER: &str = ".prometheus-bridge";

/// Where the bridge runtime lives.
#[must_use]
pub fn dir(home: &Path) -> PathBuf {
    home.join("runtimes").join("whatsapp-bridge")
}

/// The digest of the shipped bridge.
fn digest() -> String {
    let mut hash = Sha256::new();
    for (name, text) in FILES {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(text.as_bytes());
    }
    pmt_gateway::messaging::sign::hex(&hash.finalize())
}

/// Whether the shipped bridge is installed with its dependencies.
#[must_use]
pub fn installed(home: &Path) -> bool {
    let dir = dir(home);
    std::fs::read_to_string(dir.join(MARKER)).is_ok_and(|m| m.trim() == digest())
        && dir.join("node_modules").is_dir()
}

/// npm's command-line script inside the Node runtime whose executable is
/// `node` (`bin/node` with `lib/node_modules` on Unix, `node.exe` with
/// `node_modules` on Windows).
fn npm_cli(node: &Path) -> Option<PathBuf> {
    let bin = node.parent()?;
    [
        bin.parent()?.join("lib/node_modules/npm/bin/npm-cli.js"),
        bin.join("node_modules/npm/bin/npm-cli.js"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

fn io(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::RUNTIME_IO, path, err)
}

/// Installs the shipped bridge with `node`'s npm (a no-op when it is
/// installed); `progress` hears each step.
///
/// # Errors
///
/// `runtime.io` when the files cannot be written; `runtime.download` when
/// npm cannot be found or `npm ci` fails (its output is in the message).
pub fn install(home: &Path, node: &Path, progress: &dyn Fn(&str)) -> Result<(), Error> {
    if installed(home) {
        return Ok(());
    }
    let dir = dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| io(&dir, &e))?;
    for (name, text) in FILES {
        let path = dir.join(name);
        std::fs::write(&path, text).map_err(|e| io(&path, &e))?;
    }
    let npm = npm_cli(node).ok_or_else(|| {
        Error::new(
            &codes::RUNTIME_DOWNLOAD,
            format!("no npm next to the Node runtime at {}", node.display()),
        )
    })?;
    progress("installing the WhatsApp bridge's packages (npm ci)");
    let out = std::process::Command::new(node)
        .arg(&npm)
        .args([
            "ci",
            "--omit=dev",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
        ])
        .current_dir(&dir)
        .output()
        .map_err(|e| io(&npm, &e))?;
    if !out.status.success() {
        return Err(Error::new(
            &codes::RUNTIME_DOWNLOAD,
            format!(
                "npm ci failed for the WhatsApp bridge: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    let marker = dir.join(MARKER);
    std::fs::write(&marker, digest()).map_err(|e| io(&marker, &e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_bridge_has_its_lockfile_and_pairing_code_support() {
        let names: Vec<&str> = FILES.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"package-lock.json"));
        let bridge = FILES.iter().find(|(n, _)| *n == "bridge.js").unwrap().1;
        assert!(bridge.contains("requestPairingCode"));
        assert_eq!(digest().len(), 64);
    }

    /// The install runs `npm ci` with the runtime's own npm, in the
    /// bridge's directory, then marks the bridge installed (a stand-in
    /// Node that "installs" by creating `node_modules`).
    #[cfg(unix)]
    #[test]
    fn the_bridge_is_written_and_its_packages_installed_with_the_runtimes_npm() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().unwrap();
        let runtime = home.path().join("node-runtime");
        std::fs::create_dir_all(runtime.join("bin")).unwrap();
        let npm = runtime.join("lib/node_modules/npm/bin");
        std::fs::create_dir_all(&npm).unwrap();
        std::fs::write(npm.join("npm-cli.js"), "").unwrap();
        let node = runtime.join("bin/node");
        std::fs::write(
            &node,
            "#!/bin/sh\necho \"$@\" > npm-args\nmkdir -p node_modules\n",
        )
        .unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
        install(home.path(), &node, &|_| {}).unwrap();
        assert!(installed(home.path()));
        assert!(dir(home.path()).join("bridge.js").is_file());
        let args = std::fs::read_to_string(dir(home.path()).join("npm-args")).unwrap();
        assert!(
            args.contains("npm-cli.js ci --omit=dev --ignore-scripts"),
            "{args}"
        );
    }

    #[test]
    fn a_marker_without_packages_is_not_installed() {
        let home = tempfile::tempdir().unwrap();
        assert!(!installed(home.path()));
        std::fs::create_dir_all(dir(home.path())).unwrap();
        std::fs::write(dir(home.path()).join(MARKER), digest()).unwrap();
        assert!(!installed(home.path()), "node_modules missing");
        std::fs::create_dir_all(dir(home.path()).join("node_modules")).unwrap();
        assert!(installed(home.path()));
    }
}
