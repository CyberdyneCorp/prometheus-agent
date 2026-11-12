//! Offline bundles (spec: distribution-and-update, Offline bundles): one
//! uncompressed tar holding everything an air-gapped host needs to install
//! or update one target, every file signed or pinned:
//!
//! ```text
//! bundle.json                              BundleManifest
//! <tag>/release-manifest.json(.minisig)    the signed release manifest
//! <tag>/prometheus-<target>.tar.gz(.minisig)
//! <tag>/runtime-index.json(.minisig)       with runtimes only
//! runtimes/<platform>/<name>/<archive>     pinned by that index
//! ```
//!
//! Plain tar so `install.sh --bundle` needs nothing but `tar`; `bundle.json`
//! is informative only: what is installed is decided by the signed files
//! (signatures against the embedded keys, archives against the signed
//! manifest, runtime archives against the installing binary's own pins).

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::fetch::Releases;
use crate::manifest::{MANIFEST_FILE, ReleaseManifest, SIGNATURE_SUFFIX, sha256_hex};
use crate::runtimes::index::RuntimeIndex;
use crate::version::{Channel, ReleaseVersion};

/// `schema` of [`BundleManifest`].
pub const BUNDLE_SCHEMA: &str = "prometheus.bundle.v1";
/// The bundle's own description.
pub const BUNDLE_FILE: &str = "bundle.json";
/// The signed runtime index each release publishes beside its manifest.
pub const RUNTIME_INDEX_FILE: &str = "runtime-index.json";

/// `bundle.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleManifest {
    /// [`BUNDLE_SCHEMA`].
    pub schema: String,
    /// The release it carries.
    pub version: ReleaseVersion,
    /// The target of its binary archive.
    pub target: String,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// The runtimes it carries.
    pub runtimes: Vec<String>,
}

/// What `prometheus bundle create` puts in a bundle.
#[derive(Debug, Clone)]
pub struct Request {
    /// The release (`None`: the channel's newest).
    pub version: Option<ReleaseVersion>,
    /// The channel `version: None` reads.
    pub channel: Channel,
    /// The binary's target triple.
    pub target: String,
    /// Runtimes to include (`node`, `cpython`, `chromium`, ...).
    pub runtimes: Vec<String>,
}

/// Downloads and verifies everything `request` names and writes the
/// bundle to `out` (atomically).
///
/// # Errors
///
/// `update.failed` (download, a release without that target, a runtime
/// without an archive for it), `update.signature` (77).
///
/// # Panics
///
/// Never: the bundle manifest always serializes.
pub async fn create(
    releases: &Releases,
    request: &Request,
    out: &Path,
    progress: &dyn Fn(&str),
) -> Result<BundleManifest, Error> {
    let version = match request.version {
        Some(version) => version,
        None => releases.channel_index(request.channel).await?.version,
    };
    let tag = version.to_string();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    progress(&format!("release {tag} manifest"));
    let (manifest_bytes, signature) = releases
        .verified_with_signature(&format!("{tag}/{MANIFEST_FILE}"))
        .await?;
    let manifest: ReleaseManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| failed(format!("{tag}/{MANIFEST_FILE} is malformed: {e}")))?;
    push_signed(
        &mut files,
        &format!("{tag}/{MANIFEST_FILE}"),
        manifest_bytes,
        signature,
    );
    let artifact = manifest
        .artifacts
        .iter()
        .find(|a| a.target == request.target)
        .ok_or_else(|| {
            failed(format!(
                "release {tag} has no archive for {}",
                request.target
            ))
        })?;
    progress(&format!(
        "downloading {} ({} bytes)",
        artifact.name, artifact.size
    ));
    let path = format!("{tag}/{}", artifact.name);
    let (archive, signature) = releases.verified_with_signature(&path).await?;
    if !artifact.matches(&archive) {
        return Err(crate::trust::failed(&artifact.name));
    }
    push_signed(&mut files, &path, archive, signature);
    if !request.runtimes.is_empty() {
        add_runtimes(releases, request, &tag, &mut files, progress).await?;
    }
    let bundle = BundleManifest {
        schema: BUNDLE_SCHEMA.to_owned(),
        version,
        target: request.target.clone(),
        created_at: jiff::Timestamp::now().to_string(),
        runtimes: request.runtimes.clone(),
    };
    let description = serde_json::to_vec_pretty(&bundle).expect("a bundle manifest serializes");
    files.insert(0, (BUNDLE_FILE.to_owned(), description));
    write_tar(out, &files)?;
    Ok(bundle)
}

async fn add_runtimes(
    releases: &Releases,
    request: &Request,
    tag: &str,
    files: &mut Vec<(String, Vec<u8>)>,
    progress: &dyn Fn(&str),
) -> Result<(), Error> {
    let path = format!("{tag}/{RUNTIME_INDEX_FILE}");
    let (bytes, signature) = releases.verified_with_signature(&path).await?;
    let index: RuntimeIndex =
        serde_json::from_slice(&bytes).map_err(|e| failed(format!("{path} is malformed: {e}")))?;
    push_signed(files, &path, bytes, signature);
    for platform in runtime_platforms(&request.target) {
        for name in &request.runtimes {
            let pin = index.pin(name)?;
            let Some(artifact) = pin.artifacts.get(platform) else {
                continue;
            };
            progress(&format!(
                "downloading {name} {} for {platform}",
                pin.version
            ));
            let data = releases.fetch_url(&artifact.url).await?;
            let actual = sha256_hex(&data);
            if actual != artifact.sha256 {
                return Err(Error::new(
                    &codes::RUNTIME_DIGEST_MISMATCH,
                    format!(
                        "sha256 mismatch for {}: pinned {}, got {actual}",
                        artifact.url, artifact.sha256
                    ),
                ));
            }
            let at = format!("runtimes/{platform}/{name}/{}", artifact.file_name());
            files.push((at, data));
        }
    }
    Ok(())
}

/// The runtime platforms a binary of `target` may run on: a musl binary
/// also runs on glibc hosts, whose runtimes are the glibc builds.
#[must_use]
pub fn runtime_platforms(target: &str) -> Vec<&'static str> {
    let arm = target.starts_with("aarch64");
    let pick = |x64: &'static str, arm64: &'static str| if arm { arm64 } else { x64 };
    if target == crate::manifest::MACOS_UNIVERSAL {
        vec!["darwin-arm64", "darwin-x64"]
    } else if target.ends_with("-apple-darwin") {
        vec![pick("darwin-x64", "darwin-arm64")]
    } else if target.ends_with("-windows-msvc") {
        vec![pick("windows-x64", "windows-arm64")]
    } else if target.ends_with("-linux-musl") {
        vec![
            pick("linux-x64-musl", "linux-arm64-musl"),
            pick("linux-x64", "linux-arm64"),
        ]
    } else if target.ends_with("-linux-gnu") {
        vec![pick("linux-x64", "linux-arm64")]
    } else {
        Vec::new()
    }
}

fn push_signed(files: &mut Vec<(String, Vec<u8>)>, path: &str, data: Vec<u8>, signature: String) {
    files.push((path.to_owned(), data));
    files.push((format!("{path}{SIGNATURE_SUFFIX}"), signature.into_bytes()));
}

fn write_tar(out: &Path, files: &[(String, Vec<u8>)]) -> Result<(), Error> {
    let temp = out.with_extension("bundle.part");
    let written = (|| -> std::io::Result<()> {
        let mut tar = tar::Builder::new(std::fs::File::create(&temp)?);
        for (path, data) in files {
            let mut header = tar::Header::new_ustar();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, data.as_slice())?;
        }
        tar.into_inner()?.sync_all()?;
        std::fs::rename(&temp, out)
    })();
    written.map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        Error::io(&codes::UPDATE_FAILED, out, &e)
    })
}

/// A bundle unpacked into a staging directory (removed on drop).
#[derive(Debug)]
pub struct Opened {
    /// Where its files are.
    pub dir: PathBuf,
    /// Its `bundle.json`.
    pub manifest: BundleManifest,
}

impl Drop for Opened {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Unpacks the bundle at `path` under `staging` (a fresh directory).
///
/// # Errors
///
/// `update.failed` for an unreadable or malformed bundle.
pub fn open(path: &Path, staging: &Path) -> Result<Opened, Error> {
    let dir = staging.join(ulid::Ulid::generate().to_string());
    crate::install::create_dirs(&dir)?;
    let opened = |manifest| Opened {
        dir: dir.clone(),
        manifest,
    };
    let unpacked = std::fs::File::open(path)
        .and_then(|file| tar::Archive::new(file).unpack(&dir))
        .map_err(|e| failed(format!("cannot read bundle {}: {e}", path.display())));
    if let Err(err) = unpacked {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(err);
    }
    let description = std::fs::read(dir.join(BUNDLE_FILE))
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            serde_json::from_slice::<BundleManifest>(&bytes).map_err(|e| e.to_string())
        });
    match description {
        Ok(manifest) => Ok(opened(manifest)),
        Err(why) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(failed(format!(
                "{} is not a prometheus bundle: {why}",
                path.display()
            )))
        }
    }
}

/// Fails unless a bundle for `bundle_target` installs on `host_target`
/// (the same target, or the macOS universal archive on a Mac).
///
/// # Errors
///
/// `update.failed` (exit 1) `bundle target <t> does not match this host
/// (<host>)`.
pub fn check_target(bundle_target: &str, host_target: &str) -> Result<(), Error> {
    let universal_on_mac =
        bundle_target == crate::manifest::MACOS_UNIVERSAL && host_target.ends_with("-apple-darwin");
    if bundle_target == host_target || universal_on_mac {
        Ok(())
    } else {
        Err(failed(format!(
            "bundle target {bundle_target} does not match this host ({host_target})"
        )))
    }
}

fn failed(message: String) -> Error {
    Error::new(&codes::UPDATE_FAILED, message)
}

#[cfg(test)]
mod tests {
    use super::{check_target, runtime_platforms};

    #[test]
    fn a_bundle_for_another_target_is_refused() {
        let err = check_target("aarch64-apple-darwin", "x86_64-unknown-linux-musl").unwrap_err();
        assert_eq!(
            err.message(),
            "bundle target aarch64-apple-darwin does not match this host (x86_64-unknown-linux-musl)"
        );
        assert!(check_target("universal2-apple-darwin", "aarch64-apple-darwin").is_ok());
        assert!(check_target("x86_64-unknown-linux-musl", "x86_64-unknown-linux-musl").is_ok());
    }

    #[test]
    fn musl_bundles_carry_glibc_runtimes_too() {
        assert_eq!(
            runtime_platforms("x86_64-unknown-linux-musl"),
            ["linux-x64-musl", "linux-x64"]
        );
        assert_eq!(runtime_platforms("aarch64-apple-darwin"), ["darwin-arm64"]);
        assert!(runtime_platforms("aarch64-linux-android").is_empty());
    }
}
