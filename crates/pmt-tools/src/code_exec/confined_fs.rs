//! File access in directories the confined script can write (the RPC
//! channel and the kernel's cells).
//!
//! The host runs unsandboxed, so it must never follow a path the script
//! controls: a symbolic link planted at an answer or output file would
//! make the host read or overwrite a file the sandbox denies the script,
//! and a FIFO planted at a request would block the host. These helpers
//! read only regular files, opened without following links and without
//! blocking, and create files only where nothing exists (`O_EXCL` /
//! `CREATE_NEW` never follows a link).

use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::Path;

/// Reads `path` when it is a regular file; a link, FIFO, device or
/// directory is refused with `InvalidInput`.
///
/// # Errors
///
/// `NotFound` when missing, `InvalidInput` when not a regular file, other
/// I/O errors.
pub(super) fn read_regular(path: &Path) -> io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    // Non-blocking so opening a FIFO returns at once (then is refused).
    no_follow(&mut options, true);
    let mut file = options.open(path)?;
    // The handle's own metadata: the path may have been swapped since.
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file; refusing to read it",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Replaces whatever is at `path` (a link is removed, never followed)
/// with a new file holding `bytes`.
///
/// # Errors
///
/// Any I/O error; `AlreadyExists` when something reappeared at `path`
/// between the removal and the creation.
pub(super) fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    no_follow(&mut options, false);
    let mut file: File = options.open(path)?;
    file.write_all(bytes)
}

/// Opens a link itself rather than its target (`O_NOFOLLOW` fails on
/// one; Windows opens the reparse point, which is not a regular file).
fn no_follow(options: &mut OpenOptions, nonblocking: bool) {
    #[cfg(unix)]
    {
        use nix::fcntl::OFlag;
        let mut flags = OFlag::O_NOFOLLOW;
        if nonblocking {
            flags |= OFlag::O_NONBLOCK;
        }
        std::os::unix::fs::OpenOptionsExt::custom_flags(options, flags.bits());
    }
    #[cfg(windows)]
    {
        /// `FILE_FLAG_OPEN_REPARSE_POINT`.
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let _ = nonblocking;
        std::os::windows::fs::OpenOptionsExt::custom_flags(options, OPEN_REPARSE_POINT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_files_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.out");
        write_new(&path, b"one").unwrap();
        write_new(&path, b"two").unwrap();
        assert_eq!(read_regular(&path).unwrap(), b"two");
        assert_eq!(
            read_regular(&dir.path().join("missing"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn links_are_neither_read_through_nor_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, "private").unwrap();
        let link = dir.path().join("cell.out");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert!(read_regular(&link).is_err());
        write_new(&link, b"model text").unwrap();
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "private");
        assert_eq!(read_regular(&link).unwrap(), b"model text");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("x.req");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        assert_eq!(
            read_regular(&fifo).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
