//! The install receipt (spec: distribution-and-update, Install receipt):
//! `install.json` beside the binary, or under
//! `<home>/updates/install/<sha16 of binary path>.json` when that directory
//! is not writable. It records who updates this binary; a binary without
//! one is a source build (`dev`).

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::manifest::sha256_hex;
use crate::version::Channel;

/// File name of a receipt beside its binary.
pub const RECEIPT_FILE: &str = "install.json";

/// Who updates an install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Owner {
    /// `install.sh` / `install.ps1`: `prometheus update` itself.
    #[serde(rename = "self")]
    SelfInstall,
    /// An offline bundle (`prometheus update --from-bundle`).
    Bundle,
    /// The Homebrew tap.
    Homebrew,
    /// The Nix flake.
    Nix,
    /// The Termux package.
    Termux,
    /// The `WinGet` manifest.
    Winget,
    /// The OCI container image.
    Image,
    /// The desktop app (dmg, `AppImage`) and its updater.
    Desktop,
    /// An MSIX package updated by App Installer.
    Msix,
    /// The Microsoft Store.
    Msstore,
    /// A `.deb` or `.rpm`.
    SystemPackage,
    /// A local source build (no receipt).
    Dev,
}

impl Owner {
    /// The receipt spelling (`self`, `homebrew`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelfInstall => "self",
            Self::Bundle => "bundle",
            Self::Homebrew => "homebrew",
            Self::Nix => "nix",
            Self::Termux => "termux",
            Self::Winget => "winget",
            Self::Image => "image",
            Self::Desktop => "desktop",
            Self::Msix => "msix",
            Self::Msstore => "msstore",
            Self::SystemPackage => "system-package",
            Self::Dev => "dev",
        }
    }

    /// Whether `prometheus update` manages this install.
    #[must_use]
    pub fn self_updates(self) -> bool {
        matches!(self, Self::SelfInstall | Self::Bundle)
    }

    /// How this install is updated, as printed after `update with:`.
    #[must_use]
    pub fn update_command(self) -> &'static str {
        match self {
            Self::SelfInstall => "prometheus update",
            Self::Bundle => "prometheus update --from-bundle <file>",
            Self::Homebrew => "brew upgrade prometheus",
            Self::Nix => "your Nix configuration (nix flake update, then rebuild)",
            Self::Termux => "pkg upgrade prometheus",
            Self::Winget => "winget upgrade CyberdyneCorp.Prometheus",
            Self::Image => "pull the new image and recreate the container",
            Self::Desktop => "the desktop app's updater",
            Self::Msix => "App Installer",
            Self::Msstore => "Microsoft Store",
            Self::SystemPackage => "the system package manager",
            Self::Dev => "rebuild from source",
        }
    }
}

/// `install.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallReceipt {
    /// Who updates this install.
    pub owner: Owner,
    /// The persisted release channel.
    pub channel: Channel,
    /// The installed release tag.
    pub version: String,
    /// The installed archive's target.
    pub target: String,
    /// RFC 3339 time of the first install.
    pub installed_at: String,
    /// The binary this receipt describes.
    pub binary: PathBuf,
}

impl InstallReceipt {
    /// The receipt of `binary`: beside it, else in the home fallback;
    /// `None` when neither exists (a `dev` build).
    ///
    /// # Errors
    ///
    /// `update.failed` when a receipt exists but cannot be read or parsed.
    pub fn read(binary: &Path, home: &Path) -> Result<Option<Self>, Error> {
        for path in [beside(binary), fallback(binary, home)] {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    return serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                        Error::new(
                            &codes::UPDATE_FAILED,
                            format!("install receipt {} is invalid: {e}", path.display()),
                        )
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_error(&path, &e)),
            }
        }
        Ok(None)
    }

    /// Writes the receipt beside the binary when that directory is
    /// writable, else to the home fallback; returns the path written.
    ///
    /// # Errors
    ///
    /// `update.failed` when neither location can be written.
    ///
    /// # Panics
    ///
    /// Never: the receipt always serializes.
    pub fn write(&self, home: &Path) -> Result<PathBuf, Error> {
        let json = serde_json::to_vec_pretty(self).expect("receipt serializes");
        let beside = beside(&self.binary);
        if private_fs::write_atomic(&beside, &json).is_ok() {
            return Ok(beside);
        }
        let fallback = fallback(&self.binary, home);
        if let Some(dir) = fallback.parent() {
            create_dirs(dir)?;
        }
        private_fs::write_atomic(&fallback, &json).map_err(|e| io_error(&fallback, &e))?;
        Ok(fallback)
    }
}

/// `install.json` beside `binary`.
#[must_use]
pub fn beside(binary: &Path) -> PathBuf {
    binary.with_file_name(RECEIPT_FILE)
}

/// `<home>/updates/install/<first 16 hex of sha256(binary path)>.json`.
#[must_use]
pub fn fallback(binary: &Path, home: &Path) -> PathBuf {
    let digest = sha256_hex(binary.to_string_lossy().as_bytes());
    home.join("updates")
        .join("install")
        .join(format!("{}.json", &digest[..16]))
}

pub(crate) fn create_dirs(dir: &Path) -> Result<(), Error> {
    let mut missing = Vec::new();
    let mut cursor = Some(dir);
    while let Some(d) = cursor.filter(|d| !d.exists()) {
        missing.push(d);
        cursor = d.parent();
    }
    for d in missing.into_iter().rev() {
        private_fs::create_dir(d).map_err(|e| io_error(d, &e))?;
    }
    Ok(())
}

pub(crate) fn io_error(path: &Path, e: &std::io::Error) -> Error {
    Error::new(&codes::UPDATE_FAILED, format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{InstallReceipt, Owner, beside, fallback};
    use crate::version::Channel;

    fn receipt(binary: &std::path::Path) -> InstallReceipt {
        InstallReceipt {
            owner: Owner::SelfInstall,
            channel: Channel::Stable,
            version: "v1.4.0".to_owned(),
            target: "x86_64-unknown-linux-musl".to_owned(),
            installed_at: "2026-10-31T00:00:00Z".to_owned(),
            binary: binary.to_owned(),
        }
    }

    #[test]
    fn missing_receipt_is_a_dev_build() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("prometheus");
        assert_eq!(InstallReceipt::read(&binary, dir.path()).unwrap(), None);
    }

    #[test]
    fn receipt_is_written_beside_the_binary_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("prometheus");
        let written = receipt(&binary).write(&dir.path().join("home")).unwrap();
        assert_eq!(written, beside(&binary));
        let text = std::fs::read_to_string(&written).unwrap();
        assert!(text.contains("\"owner\": \"self\""), "{text}");
        let back = InstallReceipt::read(&binary, &dir.path().join("home")).unwrap();
        assert_eq!(back, Some(receipt(&binary)));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_binary_directory_uses_the_home_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let binary = bin_dir.join("prometheus");
        let home = dir.path().join("home");
        let written = receipt(&binary).write(&home).unwrap();
        std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(written, fallback(&binary, &home));
        assert!(written.starts_with(home.join("updates/install")));
        assert!(InstallReceipt::read(&binary, &home).unwrap().is_some());
    }

    #[test]
    fn owner_names_round_trip() {
        for owner in [
            Owner::SelfInstall,
            Owner::Homebrew,
            Owner::SystemPackage,
            Owner::Dev,
        ] {
            let json = serde_json::to_string(&owner).unwrap();
            assert_eq!(json, format!("\"{}\"", owner.as_str()));
        }
        assert!(Owner::Bundle.self_updates());
        assert!(!Owner::Image.self_updates());
    }
}
