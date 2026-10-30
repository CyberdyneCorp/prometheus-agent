//! Putting verified member files in place, shared by home restore and
//! snapshot restore: databases through the SQLite backup API (open
//! connections see the restored pages), other files by an atomic rename.

use std::path::Path;

use pmt_core::error::Error;
use pmt_core::private_fs;

use crate::fsutil::{self, io_error};
use crate::{members, sqlite};

/// Replaces `target` with the verified `staged` copy of member `relative`.
///
/// # Errors
///
/// `backup.io`.
pub(crate) fn member(staged: &Path, target: &Path, relative: &str) -> Result<(), Error> {
    if let Some(parent) = target.parent() {
        private_fs::create_dir(parent).map_err(|e| io_error(parent, &e))?;
    }
    if members::is_database(relative) && target.exists() {
        return sqlite::restore(staged, target);
    }
    // Staging lives inside the home, so this rename stays on one
    // filesystem and is atomic.
    std::fs::rename(staged, target).map_err(|e| io_error(target, &e))?;
    fsutil::private_file(target);
    Ok(())
}
