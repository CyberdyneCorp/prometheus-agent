//! The skill ledger and the curator's state (spec: skills, Curator audit
//! ledger, single-edit rollback, purge and reports; Curator configuration
//! and scheduling).
//!
//! Every skill mutation appends one entry: who did it, what, and for each
//! touched file the SHA-256 of its content before and after. The contents
//! themselves live in the blob store, referenced from `blob_refs` under
//! [`OWNER_KIND`], so a rollback can restore exactly the files an entry
//! touched. Appending compacts the oldest entries beyond a byte budget,
//! never the one just written.

use jiff::Timestamp;
use pmt_core::error::Error;
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::blobs::BlobRef;
use crate::error::{corrupt, sql};
use crate::sessions::now_ms;
use crate::store::Store;

/// The `blob_refs.owner_kind` of ledger contents.
pub const OWNER_KIND: &str = "skill_ledger";

/// One file an entry touched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// The path relative to the profile's `skills/` directory.
    pub path: String,
    /// SHA-256 (hex) of the content before; `None` when it did not exist.
    pub before: Option<String>,
    /// SHA-256 (hex) of the content after; `None` when it was removed.
    pub after: Option<String>,
}

/// An entry to append.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewEntry {
    /// `curator`, `agent` or `user`.
    pub actor: String,
    /// `create`, `edit`, `patch`, `delete`, `write_file`, `remove_file`,
    /// `archive`, `restore`, `purge` or `rollback`.
    pub action: String,
    /// The skill.
    pub skill: String,
    /// The skill a consolidation merged this one into.
    pub absorbed_into: Option<String>,
    /// The session that made the change.
    pub session_id: Option<String>,
    /// The touched files.
    pub files: Vec<FileChange>,
}

/// A recorded entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// The id (`L<id>` to users).
    pub id: i64,
    /// When.
    #[serde(serialize_with = "timestamp")]
    pub at: Timestamp,
    /// Who.
    pub actor: String,
    /// What.
    pub action: String,
    /// The skill.
    pub skill: String,
    /// The skill it was merged into.
    pub absorbed_into: Option<String>,
    /// The session.
    pub session_id: Option<String>,
    /// The touched files.
    pub files: Vec<FileChange>,
}

fn timestamp<S: serde::Serializer>(at: &Timestamp, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(at)
}

impl Entry {
    /// `L<id>`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("L{}", self.id)
    }
}

/// Parses `L7` or `7`.
#[must_use]
pub fn parse_label(text: &str) -> Option<i64> {
    let digits = text
        .strip_prefix('L')
        .or_else(|| text.strip_prefix('l'))
        .unwrap_or(text);
    digits.parse().ok().filter(|id| *id > 0)
}

/// The curator's state of the profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CuratorState {
    /// The last pass (`None`: never observed).
    pub last_run_at: Option<Timestamp>,
    /// `prometheus curator pause`.
    pub paused: bool,
}

const COLUMNS: &str = "id, at, actor, action, skill, absorbed_into, session_id, files";

fn entry_from_row(row: &Row<'_>) -> Result<Entry, Error> {
    let at: i64 = row.get(1).map_err(sql)?;
    let files: String = row.get(7).map_err(sql)?;
    Ok(Entry {
        id: row.get(0).map_err(sql)?,
        at: Timestamp::from_millisecond(at).map_err(|e| corrupt("skill_ledger.at", e))?,
        actor: row.get(2).map_err(sql)?,
        action: row.get(3).map_err(sql)?,
        skill: row.get(4).map_err(sql)?,
        absorbed_into: row.get(5).map_err(sql)?,
        session_id: row.get(6).map_err(sql)?,
        files: serde_json::from_str(&files).map_err(|e| corrupt("skill_ledger.files", e))?,
    })
}

impl Store {
    /// Appends `entry`, storing `contents` (every before/after content it
    /// names) in the blob store first, then compacts the oldest entries
    /// while the ledger exceeds `max_bytes` (0 = unbounded), always
    /// keeping the new one. Returns its id.
    ///
    /// # Errors
    ///
    /// `store.io`, `store.sqlite`, `store.busy`.
    pub fn append_skill_ledger(
        &self,
        entry: &NewEntry,
        contents: &[Vec<u8>],
        max_bytes: u64,
    ) -> Result<i64, Error> {
        let mut blobs = Vec::new();
        for bytes in contents {
            blobs.push(self.put_blob(bytes)?);
        }
        let files = serde_json::to_string(&entry.files).map_err(|e| corrupt("ledger files", e))?;
        let size = files.len() + contents.iter().map(Vec::len).sum::<usize>();
        let id = self.write(|tx| {
            tx.execute(
                "INSERT INTO skill_ledger (at, actor, action, skill, absorbed_into, session_id, \
                 files, bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    now_ms(),
                    entry.actor,
                    entry.action,
                    entry.skill,
                    entry.absorbed_into,
                    entry.session_id,
                    files,
                    i64::try_from(size).unwrap_or(i64::MAX),
                ],
            )
            .map_err(sql)?;
            let id = tx.last_insert_rowid();
            for blob in &blobs {
                tx.execute(
                    "INSERT OR IGNORE INTO blob_refs (sha256, owner_kind, owner_id) \
                     VALUES (?1, ?2, ?3)",
                    params![blob.sha256(), OWNER_KIND, id.to_string()],
                )
                .map_err(sql)?;
            }
            Ok(id)
        })?;
        if max_bytes > 0 {
            self.compact_skill_ledger(max_bytes, id)?;
        }
        Ok(id)
    }

    /// Deletes the oldest entries (never `keep`) while the total exceeds
    /// `max_bytes`; returns how many were removed.
    fn compact_skill_ledger(&self, max_bytes: u64, keep: i64) -> Result<usize, Error> {
        self.write(|tx| {
            let total: i64 = tx
                .query_row(
                    "SELECT coalesce(sum(bytes), 0) FROM skill_ledger",
                    [],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            let mut excess = total - i64::try_from(max_bytes).unwrap_or(i64::MAX);
            let mut removed = 0;
            while excess > 0 {
                let oldest: Option<(i64, i64)> = tx
                    .query_row(
                        "SELECT id, bytes FROM skill_ledger WHERE id <> ?1 ORDER BY id LIMIT 1",
                        [keep],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(sql)?;
                let Some((id, bytes)) = oldest else { break };
                tx.execute("DELETE FROM skill_ledger WHERE id = ?1", [id])
                    .map_err(sql)?;
                tx.execute(
                    "DELETE FROM blob_refs WHERE owner_kind = ?1 AND owner_id = ?2",
                    params![OWNER_KIND, id.to_string()],
                )
                .map_err(sql)?;
                excess -= bytes;
                removed += 1;
            }
            Ok(removed)
        })
    }

    /// The entries, newest first, optionally of one skill, at most
    /// `limit` (0 = all).
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also for a row that does not parse).
    pub fn skill_ledger(&self, skill: Option<&str>, limit: usize) -> Result<Vec<Entry>, Error> {
        let limit = if limit == 0 {
            -1
        } else {
            i64::try_from(limit).unwrap_or(i64::MAX)
        };
        self.read(|conn| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM skill_ledger WHERE ?1 IS NULL OR skill = ?1 \
                     ORDER BY id DESC LIMIT ?2"
                ))
                .map_err(sql)?;
            let rows = statement
                .query_map(params![skill, limit], |row| Ok(entry_from_row(row)))
                .map_err(sql)?;
            rows.map(|row| row.map_err(sql)?).collect()
        })
    }

    /// Entry `id`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn skill_ledger_entry(&self, id: i64) -> Result<Option<Entry>, Error> {
        self.read(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM skill_ledger WHERE id = ?1"),
                [id],
                |row| Ok(entry_from_row(row)),
            )
            .optional()
            .map_err(sql)?
            .transpose()
        })
    }

    /// The content of a ledger file version.
    ///
    /// # Errors
    ///
    /// `store.blob_missing`, `store.blob_corrupt`, `store.io`.
    pub fn skill_ledger_content(&self, sha256: &str) -> Result<Vec<u8>, Error> {
        let blob = BlobRef::parse(&format!("blob:sha256:{sha256}"))
            .ok_or_else(|| corrupt("skill_ledger.files", "not a sha256"))?;
        self.get_blob(&blob)
    }

    /// The curator's state.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn curator_state(&self) -> Result<CuratorState, Error> {
        self.read(|conn| {
            let row: Option<(Option<i64>, i64)> = conn
                .query_row(
                    "SELECT last_run_at, paused FROM curator_state WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(sql)?;
            let Some((last, paused)) = row else {
                return Ok(CuratorState::default());
            };
            Ok(CuratorState {
                last_run_at: last
                    .map(Timestamp::from_millisecond)
                    .transpose()
                    .map_err(|e| corrupt("curator_state.last_run_at", e))?,
                paused: paused != 0,
            })
        })
    }

    /// Records the last pass time.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.busy`.
    pub fn set_curator_last_run(&self, at: Timestamp) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO curator_state (id, last_run_at) VALUES (1, ?1) \
                 ON CONFLICT (id) DO UPDATE SET last_run_at = excluded.last_run_at",
                [at.as_millisecond()],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Pauses or resumes automatic passes.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.busy`.
    pub fn set_curator_paused(&self, paused: bool) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO curator_state (id, paused) VALUES (1, ?1) \
                 ON CONFLICT (id) DO UPDATE SET paused = excluded.paused",
                [i64::from(paused)],
            )
            .map(drop)
            .map_err(sql)
        })
    }
}
