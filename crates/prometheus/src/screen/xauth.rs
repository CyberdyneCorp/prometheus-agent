//! The bot screen's X authorization: Xvnc starts with `-auth` naming a
//! private Xauthority file that holds one random MIT-MAGIC-COOKIE-1, as
//! `xvfb-run` does. Without it the X server accepts any local connection
//! to its socket in the world-reachable `/tmp/.X11-unix`, so another OS
//! user of the host could read and drive the screen.
//!
//! The file is written directly (the format is the libXau one) rather than
//! with `xauth`, which hosts need not have. Its single entry has the
//! wildcard family and an empty display number, which libXau matches for
//! any display, a display reached by a relayed socket path included.

use std::path::Path;

use pmt_core::error::{Error, codes};

/// The file's name.
pub(super) const FILE: &str = "Xauthority";
/// libXau's wildcard address family.
const FAMILY_WILD: u16 = 0xffff;
/// The authorization protocol.
const PROTOCOL: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// A fresh cookie's Xauthority file contents.
///
/// # Errors
///
/// `screen.unavailable` when the OS has no randomness to give.
pub(super) fn fresh() -> Result<Vec<u8>, Error> {
    let mut cookie = [0u8; 16];
    getrandom::fill(&mut cookie)
        .map_err(|e| super::launch::failed(&format!("no random X cookie: {e}")))?;
    Ok(entry(&cookie))
}

/// One Xauthority entry: family, then address, display number, protocol
/// name and data, each a big-endian u16 length and its bytes.
fn entry(cookie: &[u8]) -> Vec<u8> {
    let mut out = FAMILY_WILD.to_be_bytes().to_vec();
    for field in [&b""[..], b"", PROTOCOL, cookie] {
        let len = u16::try_from(field.len()).expect("Xauthority fields are short");
        out.extend(len.to_be_bytes());
        out.extend(field);
    }
    out
}

/// Writes `contents` to `path` readable by its owner only, or by everyone
/// with `shared` (a container's user may map to another uid; the private
/// socket directory around it keeps other users of this host out).
///
/// # Errors
///
/// `home.io`.
pub(super) fn write(path: &Path, contents: &[u8], shared: bool) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    let io = |e: std::io::Error| Error::io(&codes::HOME_IO, path, &e);
    pmt_core::private_fs::write_atomic(path, contents).map_err(io)?;
    if shared {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).map_err(io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_is_a_wildcard_magic_cookie() {
        let bytes = entry(&[7u8; 16]);
        let mut expected = vec![0xff, 0xff, 0, 0, 0, 0, 0, 18];
        expected.extend(b"MIT-MAGIC-COOKIE-1");
        expected.extend([0, 16]);
        expected.extend([7u8; 16]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn every_cookie_is_new_and_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let first = fresh().unwrap();
        assert_ne!(first, fresh().unwrap());
        write(&path, &first, false).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), first);
    }
}
