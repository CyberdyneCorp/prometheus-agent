//! Small per-store values the program keeps for itself in `store_meta`
//! (spec: session-storage, Low-disk prune prompt: when the prompt last
//! ran and what the user answered). Keys are namespaced `pref.` so they
//! never meet the store's own metadata.

use pmt_core::error::Error;
use rusqlite::{OptionalExtension, params};

use crate::error::sql;
use crate::store::Store;

fn key(name: &str) -> String {
    format!("pref.{name}")
}

impl Store {
    /// The value stored under `name`, if any.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn pref(&self, name: &str) -> Result<Option<String>, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT value FROM store_meta WHERE key = ?1",
                [key(name)],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)
        })
    }

    /// Stores `value` under `name`.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn set_pref(&self, name: &str, value: &str) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO store_meta (key, value) VALUES (?1, ?2) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key(name), value],
            )
            .map(drop)
            .map_err(sql)
        })
    }
}
