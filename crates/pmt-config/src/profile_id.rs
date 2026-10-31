//! A profile's stable identity (`<profile>/profile_id`, `prof_<ULID>`).
//!
//! The profile name is not an identity: `profile rename` moves the
//! directory. The id file moves with it, is never cloned
//! (`--clone-from` copies only config, identity and content dirs) and is
//! what `state.db` records in `store_meta.profile_id`, so a database copied
//! from another profile is detected (spec: session-storage, Store opened
//! only through `ProfileScope`).

use std::io;
use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_core::id::ProfileId;
use pmt_core::private_fs;

use crate::layout::ProfileDir;

/// The identity file inside a profile directory.
pub const PROFILE_ID_FILE: &str = "profile_id";

impl ProfileDir {
    /// The profile's id, created on first use.
    ///
    /// Creation is atomic across processes: the id is written to a temp
    /// file and hard-linked into place, so a concurrent reader sees either
    /// no file or the whole id, and the first creator wins.
    ///
    /// # Errors
    ///
    /// `profile.id_invalid` when the file exists but does not hold an id;
    /// `home.io`.
    pub fn id(&self) -> Result<ProfileId, Error> {
        let path = self.path().join(PROFILE_ID_FILE);
        if let Some(id) = read(&path)? {
            return Ok(id);
        }
        create(&path)?;
        read(&path)?.ok_or_else(|| invalid(&path, "file vanished after creation"))
    }
}

fn read(path: &Path) -> Result<Option<ProfileId>, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => ProfileId::parse(text.trim())
            .map(Some)
            .map_err(|e| invalid(path, e.message())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(crate::layout::io_error(path, &e)),
    }
}

fn create(path: &Path) -> Result<(), Error> {
    let temp = path.with_file_name(format!("{PROFILE_ID_FILE}.tmp.{}", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    private_fs::write_atomic(&temp, format!("{}\n", ProfileId::new()).as_bytes())
        .map_err(|e| crate::layout::io_error(&temp, &e))?;
    let linked = std::fs::hard_link(&temp, path);
    let _ = std::fs::remove_file(&temp);
    match linked {
        // Another process created it first; its id wins.
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
            Err(crate::layout::io_error(path, &e))
        }
        _ => Ok(()),
    }
}

fn invalid(path: &Path, why: &str) -> Error {
    Error::new(
        &codes::PROFILE_ID_INVALID,
        format!("{}: {why}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use crate::{Home, ProfileName};

    #[test]
    fn id_is_stable_and_survives_rename() {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(root.path()).unwrap();
        let work = home.profile(&ProfileName::parse("work").unwrap()).unwrap();
        let id = work.id().unwrap();
        assert_eq!(work.id().unwrap(), id);
        let other = home.profile(&ProfileName::parse("home").unwrap()).unwrap();
        assert_ne!(other.id().unwrap(), id);
        let renamed = home
            .rename_profile(&ProfileName::parse("work").unwrap(), "job")
            .unwrap();
        assert_eq!(renamed.id().unwrap(), id);
    }

    #[test]
    fn garbage_id_file_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(root.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        std::fs::write(profile.path().join(super::PROFILE_ID_FILE), "nope").unwrap();
        assert_eq!(profile.id().unwrap_err().code().code, "profile.id_invalid");
    }
}
