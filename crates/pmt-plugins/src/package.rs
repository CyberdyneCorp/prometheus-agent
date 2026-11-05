//! `.pmtpkg` plugin packages and their signatures (spec: plugin-system,
//! Signed plugin packages and provenance).
//!
//! A package is a zstd-compressed tar of a plugin directory (manifest,
//! component, assets, `skills/`), written in path order with fixed
//! metadata so the same directory packs to the same bytes. Its digest is
//! `sha256:` of the file; its detached minisign signature (`<file>.minisig`)
//! signs that digest text. Unpacking refuses links, absolute paths and
//! `..`, and never writes the install record a package might carry.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use pmt_core::error::{Error, codes};
use sha2::{Digest as _, Sha256};

use crate::digest::INSTALL_RECORD;

/// The package file extension.
pub const EXTENSION: &str = "pmtpkg";
/// Most bytes a package may expand to.
const MAX_UNPACKED: u64 = 512 * 1024 * 1024;

fn failed(what: impl std::fmt::Display) -> Error {
    Error::new(&codes::PLUGINS_CATALOG, what.to_string())
}

/// `sha256:<hex>` of a package's bytes.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let hash = Sha256::digest(bytes);
    hash.iter().fold("sha256:".to_owned(), |mut text, b| {
        let _ = write!(text, "{b:02x}");
        text
    })
}

/// The detached signature file of `package`.
#[must_use]
pub fn signature_path(package: &Path) -> PathBuf {
    let mut name = package.as_os_str().to_owned();
    name.push(".minisig");
    PathBuf::from(name)
}

/// Packs the plugin directory `dir` (without its install record).
///
/// # Errors
///
/// `plugins.catalog` when a file cannot be read or the directory holds a
/// link.
pub fn pack(dir: &Path) -> Result<Vec<u8>, Error> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files)?;
    files.sort();
    let mut builder = tar::Builder::new(Vec::new());
    for relative in &files {
        let path = dir.join(relative);
        let bytes =
            std::fs::read(&path).map_err(|e| Error::io(&codes::PLUGINS_CATALOG, &path, &e))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        let name = relative.to_string_lossy().replace('\\', "/");
        builder
            .append_data(&mut header, name, bytes.as_slice())
            .map_err(failed)?;
    }
    let tar = builder.into_inner().map_err(failed)?;
    zstd::encode_all(tar.as_slice(), 19).map_err(failed)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| Error::io(&codes::PLUGINS_CATALOG, dir, &e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(&codes::PLUGINS_CATALOG, dir, &e))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| Error::io(&codes::PLUGINS_CATALOG, &path, &e))?;
        if kind.is_symlink() {
            return Err(failed(format!("{} is a symbolic link", path.display())));
        }
        if kind.is_dir() {
            collect(root, &path, out)?;
        } else if !(dir == root && entry.file_name() == INSTALL_RECORD) {
            out.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
    Ok(())
}

/// Unpacks package bytes into `dest` (created empty).
///
/// # Errors
///
/// `plugins.catalog` for a package that is not a valid archive, holds a
/// link, an absolute path or `..`, or expands past 512 MiB.
pub fn unpack(bytes: &[u8], dest: &Path) -> Result<(), Error> {
    unpack_capped(bytes, dest, MAX_UNPACKED)
}

/// Unpacks while decompressing, so a package never expands in memory: the
/// file contents may total `limit` bytes, and the tar stream (contents
/// plus headers and padding) is cut a quarter past that.
fn unpack_capped(bytes: &[u8], dest: &Path, limit: u64) -> Result<(), Error> {
    let decoder = zstd::stream::read::Decoder::new(bytes)
        .map_err(|e| failed(format!("not a plugin package: {e}")))?;
    let mut archive = tar::Archive::new(decoder.take(limit + limit / 4));
    let extracted = extract(&mut archive, dest, limit);
    // A cut on a block boundary reads as a clean end of the archive, so
    // reaching the cut refuses whatever the extraction answered.
    if archive.into_inner().limit() == 0 {
        return Err(too_large());
    }
    extracted
}

fn too_large() -> Error {
    failed("the package expands past 512 MiB")
}

fn extract<R: std::io::Read>(
    archive: &mut tar::Archive<R>,
    dest: &Path,
    limit: u64,
) -> Result<(), Error> {
    let mut total = 0u64;
    for entry in archive
        .entries()
        .map_err(|e| failed(format!("not a plugin package: {e}")))?
    {
        let mut entry = entry.map_err(|e| failed(format!("not a plugin package: {e}")))?;
        let kind = entry.header().entry_type();
        let path = entry.path().map_err(failed)?.into_owned();
        if kind.is_dir() {
            continue;
        }
        let unsafe_path = path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)));
        if !kind.is_file() || unsafe_path {
            return Err(failed(format!(
                "package entry '{}' is not a plain relative file",
                path.display()
            )));
        }
        if path == Path::new(INSTALL_RECORD) {
            continue;
        }
        total += entry.size();
        if total > limit {
            return Err(too_large());
        }
        let target = dest.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(&codes::PLUGINS_CATALOG, parent, &e))?;
        }
        let mut file = std::fs::File::create(&target)
            .map_err(|e| Error::io(&codes::PLUGINS_CATALOG, &target, &e))?;
        std::io::copy(&mut entry, &mut file).map_err(failed)?;
    }
    Ok(())
}

/// Signs `digest` (a package or content digest) with a minisign secret
/// key file's text; answers the `.minisig` text.
///
/// # Errors
///
/// `plugins.signature` when the key is not an unencrypted minisign secret
/// key.
pub fn sign(digest: &str, secret_key: &str) -> Result<String, Error> {
    let invalid = |why: String| Error::new(&codes::PLUGINS_SIGNATURE, why);
    let key = minisign::SecretKeyBox::from_string(secret_key)
        .and_then(minisign::SecretKeyBox::into_unencrypted_secret_key)
        .map_err(|e| invalid(format!("not an unencrypted minisign secret key: {e}")))?;
    minisign::sign(None, &key, digest.as_bytes(), None, None)
        .map(minisign::SignatureBox::into_string)
        .map_err(|e| invalid(format!("cannot sign: {e}")))
}

/// Verifies a `.minisig` text over `digest` against a base64 minisign
/// public key; answers the key id (16 upper-case hex).
///
/// # Errors
///
/// `plugins.signature` (`signature verification failed for <what>`) when
/// the signature is malformed, by another key, or not over `digest`.
pub fn verify(
    digest: &str,
    signature: &str,
    public_key: &str,
    what: &str,
) -> Result<String, Error> {
    let refused = || {
        Error::new(
            &codes::PLUGINS_SIGNATURE,
            format!("signature verification failed for {what}"),
        )
    };
    let key = minisign_verify::PublicKey::from_base64(public_key).map_err(|_| refused())?;
    let signature = minisign_verify::Signature::decode(signature).map_err(|_| refused())?;
    key.verify(digest.as_bytes(), &signature, false)
        .map_err(|_| refused())?;
    key_id(public_key).ok_or_else(refused)
}

/// The key id of a base64 minisign public key.
#[must_use]
pub fn key_id(public_key: &str) -> Option<String> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(public_key.trim())
        .ok()?;
    let bytes: [u8; 8] = raw.get(2..10)?.try_into().ok()?;
    Some(format!("{:016X}", u64::from_le_bytes(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_packs_reproducibly_and_unpacks_to_the_same_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.toml"), "id = 'x'").unwrap();
        std::fs::create_dir_all(dir.path().join("skills/board")).unwrap();
        std::fs::write(dir.path().join("skills/board/SKILL.md"), "# board").unwrap();
        std::fs::write(dir.path().join(INSTALL_RECORD), "source = 'x'").unwrap();
        let first = pack(dir.path()).unwrap();
        assert_eq!(pack(dir.path()).unwrap(), first);
        let out = tempfile::tempdir().unwrap();
        unpack(&first, out.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.path().join("skills/board/SKILL.md")).unwrap(),
            "# board"
        );
        assert!(!out.path().join(INSTALL_RECORD).exists());
        assert!(unpack(b"not a package", out.path()).is_err());
    }

    fn refused_at(dir: &Path, limit: u64) -> String {
        let out = tempfile::tempdir().unwrap();
        unpack_capped(&pack(dir).unwrap(), out.path(), limit)
            .unwrap_err()
            .message()
            .to_owned()
    }

    #[test]
    fn a_package_whose_files_expand_past_the_cap_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.bin"), vec![0u8; 64 * 1024]).unwrap();
        assert_eq!(
            refused_at(dir.path(), 16 * 1024),
            "the package expands past 512 MiB"
        );
    }

    /// Headers and padding count too: the decompressed stream is cut, so
    /// an archive of countless empty entries never inflates in full.
    #[test]
    fn a_package_whose_tar_stream_runs_past_the_cap_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..200 {
            std::fs::write(dir.path().join(format!("empty-{n}")), b"").unwrap();
        }
        assert_eq!(
            refused_at(dir.path(), 16 * 1024),
            "the package expands past 512 MiB"
        );
    }

    #[test]
    fn signatures_verify_only_with_the_publisher_key_and_digest() {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let secret = pair.sk.to_box(None).unwrap().to_string();
        let public = pair.pk.to_base64();
        let signed = digest(b"package");
        let signature = sign(&signed, &secret).unwrap();
        let id = verify(&signed, &signature, &public, "p.pmtpkg").unwrap();
        assert_eq!(id.len(), 16);
        let other = digest(b"other");
        let err = verify(&other, &signature, &public, "p.pmtpkg").unwrap_err();
        assert_eq!(err.message(), "signature verification failed for p.pmtpkg");
        let stranger = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        assert!(verify(&signed, &signature, &stranger.pk.to_base64(), "p").is_err());
    }
}
