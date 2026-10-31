//! Well-known paths: the credential stores every default profile denies and
//! the system toolchain roots the baseline may read
//! (spec: security-boundaries, Default sandbox denies credential stores).

use std::path::{Path, PathBuf};

use crate::policy::Paths;

/// Credential stores relative to the user's home on every OS.
const HOME_STORES: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".config/gcloud",
    ".azure",
    ".kube",
    ".docker/config.json",
    ".netrc",
];

/// OS keychain files and browser profile directories under the user's home.
#[cfg(target_os = "macos")]
const HOME_OS_STORES: &[&str] = &[
    "Library/Keychains",
    "Library/Cookies",
    "Library/Safari",
    "Library/Containers/com.apple.Safari",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Chromium",
    "Library/Application Support/Firefox",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/Arc",
    "Library/Application Support/Vivaldi",
];

#[cfg(windows)]
const HOME_OS_STORES: &[&str] = &[
    "AppData/Roaming/Microsoft/Credentials",
    "AppData/Local/Microsoft/Credentials",
    "AppData/Roaming/Microsoft/Protect",
    "AppData/Local/Google/Chrome/User Data",
    "AppData/Local/Chromium/User Data",
    "AppData/Local/Microsoft/Edge/User Data",
    "AppData/Local/BraveSoftware",
    "AppData/Roaming/Mozilla/Firefox",
];

#[cfg(not(any(target_os = "macos", windows)))]
const HOME_OS_STORES: &[&str] = &[
    ".local/share/keyrings",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".config/microsoft-edge",
    ".config/vivaldi",
    ".config/opera",
    "snap/firefox",
    "snap/chromium",
];

/// System-wide keychains outside any home.
#[cfg(target_os = "macos")]
const SYSTEM_STORES: &[&str] = &["/Library/Keychains", "/Network/Library/Keychains"];

#[cfg(not(target_os = "macos"))]
const SYSTEM_STORES: &[&str] = &[];

/// Every store the default profile denies for `paths`: the lists above plus
/// `PROMETHEUS_HOME` (the session's scratch and spill directories inside it
/// stay reachable through their own grants).
pub(crate) fn credential_stores(paths: &Paths) -> Vec<PathBuf> {
    let home = HOME_STORES
        .iter()
        .chain(HOME_OS_STORES)
        .map(|rel| paths.user_home.join(rel));
    let system = SYSTEM_STORES.iter().map(PathBuf::from);
    let mut stores: Vec<PathBuf> = home
        .chain(system)
        .chain([paths.prometheus_home.clone()])
        .map(|path| canonical(&path))
        .collect();
    stores.sort();
    stores.dedup();
    stores
}

/// The system toolchain (`<toolchain>`): interpreters, compilers, shared
/// libraries and the system configuration they read. Only existing roots.
pub(crate) fn toolchain_roots() -> Vec<PathBuf> {
    TOOLCHAIN
        .iter()
        .map(Path::new)
        .filter(|path| path.exists())
        .map(canonical)
        .collect()
}

#[cfg(target_os = "macos")]
const TOOLCHAIN: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/System",
    "/Library",
    "/opt",
    "/Applications/Xcode.app",
    "/private/etc",
    "/private/var/db/xcode_select_link",
    "/private/var/select",
];

/// `AppContainer` processes already read and execute the Windows and Program Files
/// trees through the `ALL APPLICATION PACKAGES` entries of their ACLs, so
/// no root needs an extra grant.
#[cfg(windows)]
const TOOLCHAIN: &[&str] = &[];

#[cfg(not(any(target_os = "macos", windows)))]
const TOOLCHAIN: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/etc",
    "/opt",
    "/nix/store",
    "/run/current-system/sw",
];

/// `path` with symlinks resolved when it exists (capability globs are
/// matched against canonical paths), else unchanged. Windows verbatim
/// prefixes (`\\?\C:\`) are dropped so paths compare with configured ones.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    let Ok(resolved) = std::fs::canonicalize(path) else {
        return path.to_path_buf();
    };
    strip_verbatim(resolved)
}

#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_stores_are_denied() {
        let paths = Paths {
            user_home: PathBuf::from("/nonexistent/u"),
            prometheus_home: PathBuf::from("/nonexistent/u/.prometheus"),
            workspace: PathBuf::from("/nonexistent/u/w"),
            scratch: Vec::new(),
        };
        let stores = credential_stores(&paths);
        for rel in [
            ".ssh",
            ".gnupg",
            ".aws",
            ".config/gcloud",
            ".azure",
            ".kube",
            ".docker/config.json",
            ".netrc",
            ".prometheus",
        ] {
            assert!(stores.contains(&paths.user_home.join(rel)), "{rel}");
        }
        assert!(
            stores.len() > HOME_STORES.len() + 1,
            "keychains and browser profiles too"
        );
    }
}
