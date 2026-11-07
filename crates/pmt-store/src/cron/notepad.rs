//! `cron_notepad`: each job's durable key-value notepad (spec:
//! scheduled-jobs, Per-job notepad). Limits are checked by `pmt-cron`
//! inside [`Store::cron_notepad_write`]'s transaction.

use pmt_core::error::Error;
use rusqlite::params;

use crate::error::sql;
use crate::sessions::now_ms;
use crate::store::Store;

fn entries(conn: &rusqlite::Connection, job_id: &str) -> Result<Vec<(String, String)>, Error> {
    let mut stmt = conn
        .prepare("SELECT key, value FROM cron_notepad WHERE job_id = ?1 ORDER BY key")
        .map_err(sql)?;
    let rows = stmt
        .query_map(params![job_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

impl Store {
    /// The notepad of `job_id`, by key.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_notepad(&self, job_id: &str) -> Result<Vec<(String, String)>, Error> {
        self.read(|conn| entries(conn, job_id))
    }

    /// Sets (`Some`) or deletes (`None`) `key` of `job_id`'s notepad after
    /// `check` accepted the notepad as it would be; returns whether a key
    /// was there before.
    ///
    /// # Errors
    ///
    /// The error of `check` (nothing changes), `store.sqlite`.
    pub fn cron_notepad_write(
        &self,
        job_id: &str,
        key: &str,
        value: Option<&str>,
        check: impl Fn(&[(String, String)]) -> Result<(), Error>,
    ) -> Result<bool, Error> {
        self.write(|tx| {
            let mut notes = entries(tx, job_id)?;
            let existed = notes.iter().any(|(k, _)| k == key);
            notes.retain(|(k, _)| k != key);
            if let Some(value) = value {
                notes.push((key.to_owned(), value.to_owned()));
            }
            check(&notes)?;
            match value {
                Some(value) => tx.execute(
                    "INSERT OR REPLACE INTO cron_notepad (job_id, key, value, updated_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![job_id, key, value, now_ms()],
                ),
                None => tx.execute(
                    "DELETE FROM cron_notepad WHERE job_id = ?1 AND key = ?2",
                    params![job_id, key],
                ),
            }
            .map_err(sql)?;
            Ok(existed)
        })
    }
}
