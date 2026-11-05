//! The profile's PII redaction (spec: security-boundaries, Optional PII
//! redaction): `privacy.redact_pii` with the profile's hashing key,
//! `<profile>/privacy.key` (32 random bytes as hex, 0600, created once and
//! never rotated, so a user keeps the same `user_<hash>` across sessions).

use std::io::{ErrorKind, Write as _};
use std::path::Path;
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_core::pii::Pii;
use pmt_core::private_fs;

use crate::schema::PrivacyConfig;

/// The key file, relative to the profile directory.
pub const KEY_FILE: &str = "privacy.key";

/// The redaction `config` asks for in the profile at `profile_dir`; `None`
/// when `redact_pii` is off.
///
/// # Errors
///
/// `home.io` when the key cannot be read or created.
pub fn pii(profile_dir: &Path, config: &PrivacyConfig) -> Result<Option<Arc<Pii>>, Error> {
    if !config.redact_pii {
        return Ok(None);
    }
    let path = profile_dir.join(KEY_FILE);
    let key = match read_key(&path) {
        Ok(Some(key)) => key,
        Ok(None) => create_key(&path)?,
        Err(err) => return Err(Error::io(&codes::HOME_IO, &path, &err)),
    };
    Ok(Some(Arc::new(Pii::new(key, &config.owner_emails))))
}

fn read_key(path: &Path) -> std::io::Result<Option<[u8; 32]>> {
    let text = match private_fs::read_no_follow(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    decode(text.trim())
        .map(Some)
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidData, "not a 64-hex-digit key"))
}

/// Creates the key; when another process created it first, reads theirs.
fn create_key(path: &Path) -> Result<[u8; 32], Error> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).expect("the OS random generator is available");
    let hex = key.iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
        out
    });
    let written = private_fs::new_file_options()
        .open(path)
        .and_then(|mut file| file.write_all(hex.as_bytes()));
    match written {
        Ok(()) => Ok(key),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => read_key(path)
            .and_then(|k| k.ok_or_else(|| ErrorKind::NotFound.into()))
            .map_err(|e| Error::io(&codes::HOME_IO, path, &e)),
        Err(err) => Err(Error::io(&codes::HOME_IO, path, &err)),
    }
}

fn decode(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::pii;
    use crate::schema::PrivacyConfig;

    #[test]
    fn the_key_is_created_once_and_hashes_stay_stable() {
        let dir = tempfile::tempdir().unwrap();
        let on = PrivacyConfig {
            redact_pii: true,
            owner_emails: Vec::new(),
        };
        assert!(
            pii(dir.path(), &PrivacyConfig::default())
                .unwrap()
                .is_none()
        );
        let first = pii(dir.path(), &on).unwrap().unwrap().user_hash("42424242");
        let again = pii(dir.path(), &on).unwrap().unwrap().user_hash("42424242");
        assert_eq!(first, again);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(super::KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
