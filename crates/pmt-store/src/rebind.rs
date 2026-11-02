//! Moving a copied `state.db` to another profile (spec: configuration-and-
//! profiles, Profile lifecycle commands: `profile import` of an archive
//! with sessions). A store records the id and name of the profile it
//! belongs to and refuses to open under any other; a copy imported as a new
//! profile is rebound to that profile's identity before its first open.

use std::path::Path;

use pmt_config::ProfileDir;
use pmt_core::error::Error;
use rusqlite::{Connection, params};

use crate::error::sql;
use crate::location::DATABASE_FILE;

/// Rebinds `profile`'s copied `state.db` (when it has one) to `profile`:
/// the recorded profile id and name, and every session's profile id.
///
/// # Errors
///
/// `store.sqlite` when the file cannot be updated; the profile id error.
pub fn adopt_copy(profile: &ProfileDir) -> Result<(), Error> {
    let database = profile.path().join(DATABASE_FILE);
    if !database.is_file() {
        return Ok(());
    }
    let id = profile.id()?.to_string();
    rebind(&database, &id, profile.name().as_str())
}

fn rebind(database: &Path, id: &str, name: &str) -> Result<(), Error> {
    let mut conn = Connection::open(database).map_err(sql)?;
    let tx = conn.transaction().map_err(sql)?;
    tx.execute(
        "UPDATE store_meta SET value = ?1 WHERE key = 'profile_id'",
        params![id],
    )
    .map_err(sql)?;
    tx.execute(
        "UPDATE store_meta SET value = ?1 WHERE key = 'profile_name'",
        params![name],
    )
    .map_err(sql)?;
    tx.execute("UPDATE sessions SET profile_id = ?1", params![id])
        .map_err(sql)?;
    tx.commit().map_err(sql)
}
