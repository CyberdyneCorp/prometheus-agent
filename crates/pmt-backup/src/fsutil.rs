//! Small filesystem helpers shared by backups and snapshots.

use std::io::{self, Read};
use std::path::Path;

use pmt_core::error::{Error, codes};
use sha2::{Digest as _, Sha256};

/// Lowercase hex of `bytes`.
#[must_use]
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// A reader that hashes and counts what passes through it.
pub(crate) struct Hashing<R> {
    inner: R,
    hasher: Sha256,
    bytes: u64,
}

impl<R: Read> Hashing<R> {
    /// Wraps `inner`.
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    /// `(bytes read, hex SHA-256)`.
    pub(crate) fn finish(self) -> (u64, String) {
        (self.bytes, hex(&self.hasher.finalize()))
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}

/// `(size, hex SHA-256)` of the file at `path`.
///
/// # Errors
///
/// `backup.io`.
pub(crate) fn hash_file(path: &Path) -> Result<(u64, String), Error> {
    let file = std::fs::File::open(path).map_err(|e| io_error(path, &e))?;
    let mut reader = Hashing::new(file);
    io::copy(&mut reader, &mut io::sink()).map_err(|e| io_error(path, &e))?;
    Ok(reader.finish())
}

/// Sets mode 0600 on POSIX (files SQLite or tar created with the umask).
pub(crate) fn private_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// A `backup.io` error for `path`.
#[must_use]
pub(crate) fn io_error(path: &Path, e: &dyn std::fmt::Display) -> Error {
    Error::new(&codes::BACKUP_IO, format!("{}: {e}", path.display()))
}
