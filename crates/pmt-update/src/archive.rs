//! Unpacks the binary from a verified release archive (`.tar.gz` on every
//! target; Windows ships `tar.exe` since 1803) into the staging directory.

use std::io::Read;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use pmt_core::error::{Error, codes};

use crate::install::{create_dirs, io_error};

/// The executable's name inside an archive.
#[must_use]
pub fn binary_name() -> &'static str {
    if cfg!(windows) {
        "prometheus.exe"
    } else {
        "prometheus"
    }
}

/// Writes the archive's `prometheus` executable (at any depth: archives
/// hold it under `prometheus-<target>/`) to `staging/<name>`, executable.
///
/// # Errors
///
/// `update.failed` when the archive is unreadable or has no binary.
pub fn extract_binary(archive: &[u8], staging: &Path) -> Result<PathBuf, Error> {
    create_dirs(staging)?;
    let unreadable = |e: &dyn std::fmt::Display| {
        Error::new(
            &codes::UPDATE_FAILED,
            format!("cannot read the release archive: {e}"),
        )
    };
    let mut tar = tar::Archive::new(GzDecoder::new(archive));
    for entry in tar.entries().map_err(|e| unreadable(&e))? {
        let mut entry = entry.map_err(|e| unreadable(&e))?;
        let is_binary = entry.header().entry_type().is_file()
            && entry
                .path()
                .ok()
                .and_then(|p| p.file_name().map(|n| n == binary_name()))
                .unwrap_or(false);
        if !is_binary {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|e| unreadable(&e))?;
        let target = staging.join(binary_name());
        write_executable(&target, &bytes)?;
        return Ok(target);
    }
    Err(Error::new(
        &codes::UPDATE_FAILED,
        format!("the release archive holds no {}", binary_name()),
    ))
}

/// Writes `bytes` to `path` with mode 0755 (owner-writable, world
/// readable and executable, like any installed program).
pub(crate) fn write_executable(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::io::Write as _;
    // Synced through the writing handle: Windows refuses to flush a file
    // opened read-only.
    let mut file = std::fs::File::create(path).map_err(|e| io_error(path, &e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| io_error(path, &e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| io_error(path, &e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{binary_name, extract_binary};

    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, path, *data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn binary_is_found_below_the_top_directory() {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("prometheus-x/{}", binary_name());
        let bytes = archive(&[("prometheus-x/README.md", b"readme"), (&name, b"binary")]);
        let staged = extract_binary(&bytes, &dir.path().join("staging")).unwrap();
        assert_eq!(std::fs::read(staged).unwrap(), b"binary");
    }

    #[test]
    fn archive_without_binary_fails() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = archive(&[("README.md", b"readme")]);
        let err = extract_binary(&bytes, dir.path()).unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }
}
