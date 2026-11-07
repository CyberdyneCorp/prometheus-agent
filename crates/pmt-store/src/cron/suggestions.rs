//! `cron_suggestions`: the suggested automations inbox (spec:
//! scheduled-jobs, Suggested automations inbox). A dismissed key stays as a
//! latch so the same suggestion is never offered again.

use pmt_core::error::Error;
use rusqlite::{OptionalExtension, Row, params};

use crate::error::sql;
use crate::sessions::now_ms;
use crate::store::Store;

/// One suggestion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuggestionRow {
    /// The stable key.
    pub key: String,
    /// `catalog`, `blueprint`, `usage` or `integration`.
    pub source: String,
    /// What it offers.
    pub title: String,
    /// Its schedule, in the grammar.
    pub schedule: String,
    /// Its prompt.
    pub prompt: String,
    /// `pending`, `accepted` or `dismissed`.
    pub state: String,
    /// When it was offered.
    pub created_at: i64,
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<SuggestionRow> {
    Ok(SuggestionRow {
        key: row.get(0)?,
        source: row.get(1)?,
        title: row.get(2)?,
        schedule: row.get(3)?,
        prompt: row.get(4)?,
        state: row.get(5)?,
        created_at: row.get(6)?,
    })
}

const COLUMNS: &str = "key, source, title, schedule, prompt, state, created_at";

impl Store {
    /// Offers `row` as `pending` unless its key is known (pending,
    /// accepted or dismissed) or `max_pending` are pending already;
    /// returns whether it was added.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_offer_suggestion(
        &self,
        row: &SuggestionRow,
        max_pending: usize,
    ) -> Result<bool, Error> {
        self.write(|tx| {
            let known: Option<String> = tx
                .query_row(
                    "SELECT state FROM cron_suggestions WHERE key = ?1",
                    params![row.key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            let pending: i64 = tx
                .query_row(
                    "SELECT count(*) FROM cron_suggestions WHERE state = 'pending'",
                    [],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            if known.is_some() || usize::try_from(pending).unwrap_or(usize::MAX) >= max_pending {
                return Ok(false);
            }
            let now = now_ms();
            tx.execute(
                "INSERT INTO cron_suggestions (key, source, title, schedule, prompt, state, \
                 created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6)",
                params![
                    row.key,
                    row.source,
                    row.title,
                    row.schedule,
                    row.prompt,
                    now
                ],
            )
            .map_err(sql)?;
            Ok(true)
        })
    }

    /// The suggestions in `state`, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_suggestions(&self, state: &str) -> Result<Vec<SuggestionRow>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM cron_suggestions WHERE state = ?1 \
                     ORDER BY created_at, key"
                ))
                .map_err(sql)?;
            let rows = stmt.query_map(params![state], from_row).map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })
    }

    /// Moves suggestion `key` to `state`; `false` when there is none.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_set_suggestion(&self, key: &str, state: &str) -> Result<bool, Error> {
        self.write(|tx| {
            let n = tx
                .execute(
                    "UPDATE cron_suggestions SET state = ?2, updated_at = ?3 WHERE key = ?1",
                    params![key, state, now_ms()],
                )
                .map_err(sql)?;
            Ok(n > 0)
        })
    }

    /// Drops accepted suggestions (dismissed ones stay as latches, shown
    /// nowhere); returns how many were dropped.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_clear_suggestions(&self) -> Result<usize, Error> {
        self.write(|tx| {
            tx.execute("DELETE FROM cron_suggestions WHERE state = 'accepted'", [])
                .map_err(sql)
        })
    }
}
