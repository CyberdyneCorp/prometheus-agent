//! Installing a plugin into a profile (spec: plugin-system, Signed plugin
//! packages and provenance; plugins CLI; security-boundaries, Signed
//! plugins).
//!
//! A plugin is first staged in the profile's `cache/plugins/staging/`
//! (unpacked or checked out, its manifest validated and its content digest
//! computed), its signature is checked against the policy, and only then
//! is it moved to `<profile_home>/plugins/<id>/` with its install record.
//! Nothing half-installed ever sits where discovery looks.

use std::path::{Path, PathBuf};

use pmt_config::schema::SignaturePolicy;
use pmt_core::error::{Error, codes};
use semver::Version;

use crate::digest;
use crate::manifest::Manifest;
use crate::package;
use crate::record::InstallRecord;

/// A detached signature over a git checkout's content digest, at the
/// checkout's root (removed before the digest is computed).
pub const TREE_SIGNATURE: &str = "plugin.minisig";

/// The message of an unsigned package the policy refuses.
pub const UNSIGNED: &str = "package is unsigned; re-run with --allow-unsigned to install anyway";

/// A plugin staged for installation.
#[derive(Debug)]
pub struct Staged {
    /// Its staging directory.
    pub dir: PathBuf,
    /// Its manifest.
    pub manifest: Manifest,
    /// Its content digest.
    pub digest: String,
}

impl Drop for Staged {
    fn drop(&mut self) {
        // Committing moved the directory; anything left is abandoned.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A fresh staging directory under `<profile>/cache/plugins/staging/`.
///
/// # Errors
///
/// `plugins.catalog` when it cannot be created.
pub fn staging_dir(profile_dir: &Path) -> Result<PathBuf, Error> {
    let root = profile_dir.join("cache/plugins/staging");
    std::fs::create_dir_all(&root).map_err(|e| Error::io(&codes::PLUGINS_CATALOG, &root, &e))?;
    let dir = tempfile::Builder::new()
        .prefix("stage-")
        .tempdir_in(&root)
        .map_err(|e| Error::io(&codes::PLUGINS_CATALOG, &root, &e))?;
    Ok(dir.keep())
}

/// Stages package bytes.
///
/// # Errors
///
/// `plugins.catalog` for an unreadable package; `plugins.manifest` for an
/// invalid manifest.
pub fn stage_package(bytes: &[u8], profile_dir: &Path, running: &Version) -> Result<Staged, Error> {
    let dir = staging_dir(profile_dir)?;
    let unpacked = package::unpack(bytes, &dir);
    stage_dir(dir, running, unpacked)
}

/// Stages a directory already in the staging area (a git checkout).
///
/// # Errors
///
/// As [`stage_package`].
pub fn stage_dir(
    dir: PathBuf,
    running: &Version,
    ready: Result<(), Error>,
) -> Result<Staged, Error> {
    let cleanup = |err: Error| {
        let _ = std::fs::remove_dir_all(&dir);
        err
    };
    ready.map_err(cleanup)?;
    let manifest = if !dir.join(crate::manifest::MANIFEST_FILE).exists()
        && dir.join(crate::manifest::HERMES_MANIFEST_FILE).exists()
    {
        crate::python::hermes::read_staged(&dir).inspect(|manifest| {
            // Converted before the digest, so the record covers the TOML
            // catalogs the host reads.
            for problem in crate::locales::convert_hermes_catalogs(&dir, &manifest.provides_locales)
            {
                tracing::warn!(plugin = %manifest.id, "{problem}");
            }
        })
    } else {
        Manifest::read(&dir, running)
    }
    .map_err(cleanup)?;
    let digest = digest::of_dir(&dir).map_err(cleanup)?;
    Ok(Staged {
        dir,
        manifest,
        digest,
    })
}

/// Whether a package's signature verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    /// Verified; the signer's key id.
    Signed(String),
    /// No verifiable signature, and why.
    Unsigned(String),
}

/// Checks the detached `signature` over `digest` against the catalog's
/// `publisher_key` for the plugin.
///
/// # Errors
///
/// `plugins.signature` when a signature is present and a key is known but
/// it does not verify (always refused).
pub fn check_signature(
    digest: &str,
    signature: Option<&str>,
    publisher_key: Option<&str>,
    what: &str,
) -> Result<Trust, Error> {
    match (signature, publisher_key) {
        (None, _) => Ok(Trust::Unsigned("unsigned".to_owned())),
        (Some(_), None) => Ok(Trust::Unsigned(
            "signed by a key no catalog entry vouches for".to_owned(),
        )),
        (Some(signature), Some(key)) => {
            package::verify(digest, signature, key, what).map(Trust::Signed)
        }
    }
}

/// Applies the signature policy: catalog installs always need a verified
/// signature unless `--allow-unsigned`; local packages follow
/// `plugins.signature_policy`. Answers whether the install proceeds
/// unsigned (recorded on the install record).
///
/// # Errors
///
/// `plugins.unsigned` (exit 77): [`UNSIGNED`].
pub fn admit(
    trust: &Trust,
    catalog: bool,
    policy: SignaturePolicy,
    allow_unsigned: bool,
) -> Result<bool, Error> {
    let Trust::Unsigned(why) = trust else {
        return Ok(false);
    };
    let required = catalog || policy == SignaturePolicy::Require;
    if required && !allow_unsigned {
        return Err(Error::new(&codes::PLUGINS_UNSIGNED, UNSIGNED)
            .with_hint(format!("the package is {why}")));
    }
    if policy == SignaturePolicy::Warn || required {
        tracing::warn!("installing an unsigned plugin package ({why})");
    }
    Ok(true)
}

/// Moves a staged plugin to `<profile>/plugins/<id>/` and writes its
/// record; `replace` swaps out an installed copy (an update).
///
/// # Errors
///
/// `plugins.discovery` when the plugin is already installed (and not
/// replaced) or the move fails.
#[allow(clippy::needless_pass_by_value)] // the staged directory is consumed by the move
pub fn commit(
    staged: Staged,
    profile_dir: &Path,
    record: &InstallRecord,
    replace: bool,
) -> Result<PathBuf, Error> {
    let plugins = profile_dir.join("plugins");
    std::fs::create_dir_all(&plugins)
        .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &plugins, &e))?;
    let target = plugins.join(&staged.manifest.id);
    if target.exists() {
        if !replace {
            return Err(Error::new(
                &codes::PLUGINS_DISCOVERY,
                format!("plugin '{}' is already installed", staged.manifest.id),
            )
            .with_hint("`prometheus plugins update` replaces it"));
        }
        let old = profile_dir
            .join("cache/plugins/replaced")
            .join(&staged.manifest.id);
        let _ = std::fs::remove_dir_all(&old);
        if let Some(parent) = old.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, parent, &e))?;
        }
        std::fs::rename(&target, &old)
            .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &target, &e))?;
        let _ = std::fs::remove_dir_all(&old);
    }
    std::fs::rename(&staged.dir, &target)
        .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &target, &e))?;
    record.write(&target)?;
    Ok(target)
}

/// The current UTC time in RFC 3339 (install records).
#[must_use]
pub fn now() -> String {
    jiff::Timestamp::now().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_packages_follow_the_policy_and_catalog_installs_always_need_a_signature() {
        let unsigned = Trust::Unsigned("unsigned".to_owned());
        let err = admit(&unsigned, false, SignaturePolicy::Require, false).unwrap_err();
        assert_eq!(err.message(), UNSIGNED);
        assert_eq!(err.exit_code(), 77);
        assert!(admit(&unsigned, false, SignaturePolicy::Require, true).unwrap());
        assert!(admit(&unsigned, false, SignaturePolicy::Warn, false).unwrap());
        assert!(admit(&unsigned, false, SignaturePolicy::Off, false).unwrap());
        // The policy never relaxes a catalog install.
        assert!(admit(&unsigned, true, SignaturePolicy::Off, false).is_err());
        assert!(admit(&unsigned, true, SignaturePolicy::Off, true).unwrap());
        let signed = Trust::Signed("ABCD".to_owned());
        assert!(!admit(&signed, true, SignaturePolicy::Require, false).unwrap());
    }

    #[test]
    fn an_invalid_signature_is_refused_whatever_the_flags() {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let secret = pair.sk.to_box(None).unwrap().to_string();
        let signature = package::sign("sha256:aa", &secret).unwrap();
        let key = pair.pk.to_base64();
        assert!(matches!(
            check_signature("sha256:aa", Some(&signature), Some(&key), "p").unwrap(),
            Trust::Signed(_)
        ));
        let err = check_signature("sha256:bb", Some(&signature), Some(&key), "p").unwrap_err();
        assert_eq!(err.code(), &codes::PLUGINS_SIGNATURE);
        assert!(matches!(
            check_signature("sha256:aa", None, Some(&key), "p").unwrap(),
            Trust::Unsigned(_)
        ));
    }
}
