//! The install record `<profile_home>/plugins/<id>/.install.toml`, the only
//! authoritative provenance of an installed plugin (spec: plugin-system,
//! Signed plugin packages and provenance; plugins CLI).
//!
//! It names where the plugin came from (`catalog`, `package`, `git` or
//! `adopted`), the pinned ref, the package digest, the content digest of
//! the directory as installed, the signer and tier, when it was installed
//! and the flags that let it in (`allow_unsigned`, `allow_removed`). A
//! directory in the profile without a record was placed by hand and never
//! loads until `prometheus plugins adopt` writes one; a directory whose
//! content no longer matches its record was modified after install.

use std::path::Path;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::digest::INSTALL_RECORD;

/// Where an installed plugin came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// A catalog entry (by id).
    Catalog,
    /// A local `.pmtpkg` file.
    Package,
    /// A Git repository at a full commit SHA.
    Git,
    /// A directory placed by hand, then adopted.
    Adopted,
}

impl Origin {
    /// The record spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::Package => "package",
            Self::Git => "git",
            Self::Adopted => "adopted",
        }
    }
}

/// The flags an install was made with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // independent flags, as the record stores them
pub struct Flags {
    /// `--allow-unsigned`: installed without a verified signature.
    pub allow_unsigned: bool,
    /// `--allow-removed`: installed although on the catalog's kill list.
    pub allow_removed: bool,
    /// Installed with `plugins.scan_on_install = false`: no threat scan ran.
    pub scan_skipped: bool,
    /// `--force`: installed although the threat scan graded it `caution`.
    pub forced: bool,
}

/// One install record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRecord {
    /// Where it came from.
    pub source: Origin,
    /// The package path, repository URL or catalog id it was installed from.
    pub location: String,
    /// The pinned ref: a 40-hex commit SHA or `sha256:<package digest>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    /// `sha256:` of the `.pmtpkg` file, for package installs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_digest: Option<String>,
    /// The content digest of the directory as installed.
    pub digest: String,
    /// The minisign key id of the verified signer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    /// The catalog tier (`official`, `verified`, `community`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// When it was installed (RFC 3339).
    pub installed_at: String,
    /// The flags it was installed with.
    #[serde(default)]
    pub flags: Flags,
    /// An update the threat scan graded `dangerous`: it stays disabled,
    /// and this version active, until a different update is published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_update: Option<HeldUpdate>,
}

/// A refused (`dangerous`) update of an installed plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldUpdate {
    /// What it would have moved to: the catalog ref or the package digest.
    pub target: String,
    /// The scan's verdict (`dangerous (<pattern> in <file>:<line>)`).
    pub verdict: String,
    /// When it was refused (RFC 3339).
    pub held_at: String,
}

impl InstallRecord {
    /// The record of the plugin directory `dir`, if it has one.
    ///
    /// # Errors
    ///
    /// `plugins.discovery` when the record exists but cannot be read or
    /// parsed.
    pub fn read(dir: &Path) -> Result<Option<Self>, Error> {
        let path = dir.join(INSTALL_RECORD);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io(&codes::PLUGINS_DISCOVERY, &path, &e)),
        };
        toml::from_str(&text).map(Some).map_err(|e| {
            Error::new(
                &codes::PLUGINS_DISCOVERY,
                format!("{}: invalid install record: {e}", path.display()),
            )
        })
    }

    /// Writes the record into `dir` (atomically: a temporary file renamed
    /// over it).
    ///
    /// # Errors
    ///
    /// `plugins.discovery` when it cannot be written.
    pub fn write(&self, dir: &Path) -> Result<(), Error> {
        let path = dir.join(INSTALL_RECORD);
        let text = toml::to_string(self).map_err(|e| {
            Error::new(
                &codes::PLUGINS_DISCOVERY,
                format!("cannot write the install record: {e}"),
            )
        })?;
        let temporary = dir.join(format!("{INSTALL_RECORD}.tmp"));
        std::fs::write(&temporary, text)
            .and_then(|()| std::fs::rename(&temporary, &path))
            .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &path, &e))
    }

    /// Whether it was installed without a verified signature.
    #[must_use]
    pub fn unsigned(&self) -> bool {
        self.signer.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_a_missing_one_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(InstallRecord::read(dir.path()).unwrap(), None);
        let record = InstallRecord {
            source: Origin::Package,
            location: "/tmp/weather.pmtpkg".to_owned(),
            r#ref: None,
            package_digest: Some(format!("sha256:{}", "a".repeat(64))),
            digest: format!("sha256:{}", "b".repeat(64)),
            signer: None,
            tier: None,
            installed_at: "2026-11-03T00:00:00Z".to_owned(),
            flags: Flags {
                allow_unsigned: true,
                ..Flags::default()
            },
            held_update: None,
        };
        record.write(dir.path()).unwrap();
        assert_eq!(InstallRecord::read(dir.path()).unwrap(), Some(record));
        let text = std::fs::read_to_string(dir.path().join(INSTALL_RECORD)).unwrap();
        assert!(text.contains("source = \"package\""), "{text}");
        assert!(text.contains("allow_unsigned = true"), "{text}");
    }
}
