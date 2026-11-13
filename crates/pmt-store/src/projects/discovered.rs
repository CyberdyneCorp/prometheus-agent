//! The repository discovery cache (spec: projects, Repository discovery):
//! the git repositories a background scan found for this profile, each
//! with when a scan last saw it, and the fingerprint of the scan keys they
//! were found with. A cache of other settings reads as empty; replacing
//! it is one transaction, so a reader never sees half a scan.

use jiff::Timestamp;
use pmt_core::error::Error;
use rusqlite::params;

use crate::error::sql;
use crate::store::Store;

/// One discovered repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRepo {
    /// The repository's folder (absolute).
    pub path: String,
    /// Its folder name.
    pub name: String,
    /// When a scan last saw it.
    pub last_seen: Timestamp,
}

impl Store {
    /// The cached repositories found with the scan keys `settings`
    /// fingerprints, by path; empty when the cache holds another
    /// fingerprint's rows (or none).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn discovered_repos(&self, settings: &str) -> Result<Vec<DiscoveredRepo>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT path, name, last_seen FROM discovered_repos \
                     WHERE settings = ?1 ORDER BY path",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([settings], |row| {
                    Ok(DiscoveredRepo {
                        path: row.get(0)?,
                        name: row.get(1)?,
                        last_seen: Timestamp::from_millisecond(row.get(2)?)
                            .unwrap_or(Timestamp::UNIX_EPOCH),
                    })
                })
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })
    }

    /// Replaces the cache with what a scan under `settings` found at
    /// `seen`.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn replace_discovered_repos(
        &self,
        settings: &str,
        repos: &[(String, String)],
        seen: Timestamp,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute("DELETE FROM discovered_repos", [])
                .map_err(sql)?;
            for (path, name) in repos {
                tx.execute(
                    "INSERT INTO discovered_repos (path, name, settings, last_seen) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![path, name, settings, seen.as_millisecond()],
                )
                .map_err(sql)?;
            }
            Ok(())
        })
    }

    /// Forgets every discovered repository (scanning was turned off).
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn clear_discovered_repos(&self) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute("DELETE FROM discovered_repos", [])
                .map(drop)
                .map_err(sql)
        })
    }
}
