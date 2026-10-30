//! Owner-only store files and the filesystem check behind WAL mode (spec:
//! session-storage, Owner-only file permissions; WAL mode).

use std::io;
use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;

use crate::error;

/// Creates an empty 0600 file unless `path` exists, so SQLite opens a file
/// that never had a wider mode (SQLite gives `-wal`/`-shm` the database
/// file's mode).
pub(crate) fn create_private(path: &Path) -> Result<(), Error> {
    match private_fs::new_file_options().open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(error::io(path, &e)),
    }
}

/// Creates directory `dir` (0700) or tightens an existing one.
pub(crate) fn private_dir(dir: &Path) -> Result<(), Error> {
    private_fs::create_dir(dir).map_err(|e| error::io(dir, &e))?;
    tighten(dir, 0o700);
    Ok(())
}

/// Narrows an existing regular file or directory to `mode` when group or
/// others have any access, by `chmod` of the path; symlinks and other
/// file types are left alone. Each change is logged at WARN with the path
/// and the previous mode.
#[cfg(unix)]
pub(crate) fn tighten(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    let kind = meta.file_type();
    let previous = meta.permissions().mode() & 0o7777;
    if !(kind.is_file() || kind.is_dir()) || previous.trailing_zeros() >= 6 {
        return;
    }
    match std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        Ok(()) => tracing::warn!(
            path = %path.display(),
            previous_mode = format_args!("{previous:o}"),
            "{} had mode {previous:o}; tightened to {mode:o}",
            path.display()
        ),
        Err(e) => tracing::warn!("cannot tighten permissions of {}: {e}", path.display()),
    }
}

/// Windows owner-only ACLs are not applied yet (see the M1 notes).
#[cfg(not(unix))]
pub(crate) fn tighten(_path: &Path, _mode: u32) {}

/// Fails when WAL is unsafe on the filesystem holding `dir` (network and
/// cross-VM filesystems share no memory map between hosts).
pub(crate) fn check_wal_supported(dir: &Path) -> Result<(), Error> {
    match network_filesystem(dir) {
        None => Ok(()),
        Some(kind) => Err(Error::new(
            &codes::STORE_UNSUPPORTED_FILESYSTEM,
            format!(
                "{} is on a {kind} filesystem where SQLite WAL mode is unsafe",
                dir.display()
            ),
        )
        .with_hint("set storage.journal_mode = \"delete\" or move the home to a local disk")),
    }
}

/// The network or cross-VM filesystem kind of `dir`, if it is one.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn network_filesystem(dir: &Path) -> Option<&'static str> {
    let stat = nix::sys::statfs::statfs(dir).ok()?;
    #[allow(clippy::cast_sign_loss, clippy::unnecessary_cast)] // f_type width differs per libc
    linux_kind(stat.filesystem_type().0 as u64)
}

#[cfg(target_os = "macos")]
fn network_filesystem(dir: &Path) -> Option<&'static str> {
    let stat = nix::sys::statfs::statfs(dir).ok()?;
    named_kind(stat.filesystem_type_name())
}

#[cfg(windows)]
fn network_filesystem(dir: &Path) -> Option<&'static str> {
    windows_kind(&dir.to_string_lossy())
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
)))]
fn network_filesystem(_dir: &Path) -> Option<&'static str> {
    None
}

/// Linux `statfs.f_type` magic numbers of network and cross-VM filesystems.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn linux_kind(magic: u64) -> Option<&'static str> {
    Some(match magic {
        0x6969 => "nfs",
        0x517b => "smb",
        0xff53_4d42 => "cifs",
        0xfe53_4d42 => "smb2",
        0x0102_1997 => "9p",
        0x6a65_6a63 => "virtiofs",
        0x786f_4256 => "vboxsf",
        0x7c7c_6673 => "prl_fs",
        0x5346_414f => "afs",
        0x0000_564c => "ncp",
        _ => return None,
    })
}

/// macOS / BSD `f_fstypename` values of network and cross-VM filesystems.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn named_kind(name: &str) -> Option<&'static str> {
    const NETWORK: &[&str] = &[
        "nfs", "smbfs", "afpfs", "webdav", "cifs", "ftp", "vboxsf", "prl_fs", "9p",
    ];
    NETWORK.iter().copied().find(|kind| *kind == name)
}

/// UNC paths (`\\server\share`, `\\?\UNC\...`) are network shares.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_kind(path: &str) -> Option<&'static str> {
    let unc = path.starts_with(r"\\?\UNC\")
        || (path.starts_with(r"\\") && !path.starts_with(r"\\?\") && !path.starts_with(r"\\.\"));
    unc.then_some("network share")
}

#[cfg(test)]
mod tests {
    use super::{linux_kind, named_kind, windows_kind};

    #[test]
    fn network_filesystems_are_recognised() {
        assert_eq!(linux_kind(0x6969), Some("nfs"));
        assert_eq!(linux_kind(0x0102_1997), Some("9p"));
        assert_eq!(linux_kind(0xef53), None, "ext4 is local");
        assert_eq!(named_kind("smbfs"), Some("smbfs"));
        assert_eq!(named_kind("apfs"), None);
        assert_eq!(
            windows_kind(r"\\nas\home\prometheus"),
            Some("network share")
        );
        assert_eq!(windows_kind(r"\\?\UNC\nas\home"), Some("network share"));
        assert_eq!(windows_kind(r"\\?\C:\Users\me"), None);
        assert_eq!(windows_kind(r"C:\Users\me"), None);
    }

    #[test]
    fn local_temp_dir_supports_wal() {
        let dir = tempfile::tempdir().unwrap();
        super::check_wal_supported(dir.path()).unwrap();
    }
}
