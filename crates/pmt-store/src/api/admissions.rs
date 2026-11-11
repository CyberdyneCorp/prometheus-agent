//! Idempotent admissions of run-starting API requests (spec: api-server,
//! Idempotent admission for Chat Completions and Responses; Run
//! idempotency ledger). A key is reserved per endpoint and bearer-key
//! fingerprint before any work starts; the first reservation wins and
//! every later one sees it, with its identity hash and, once the work
//! finished, its replayable answer.

use jiff::{SignedDuration, Timestamp};
use pmt_core::error::Error;
use rusqlite::{OptionalExtension, Row, Transaction, params};

use crate::error::sql;
use crate::gateway::timestamp;
use crate::sessions::now_ms;
use crate::store::Store;

/// How long a deleted session's admissions block their keys.
pub const TOMBSTONE_FOR: SignedDuration = SignedDuration::from_hours(7 * 24);
/// How long a finished admission (and a run's key after its last status
/// change) is kept.
pub const RETAIN_FOR: SignedDuration = SignedDuration::from_hours(24);

/// The endpoint a key belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// `POST /v1/chat/completions`.
    Chat,
    /// `POST /v1/responses`.
    Responses,
    /// `POST /v1/runs`.
    Runs,
}

impl Endpoint {
    /// The stored name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
            Self::Runs => "runs",
        }
    }
}

/// One reserved key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// `adm_<ulid>`.
    pub id: String,
    /// The hash of the request's identity fields.
    pub identity: String,
    /// The session the work runs in.
    pub session_id: Option<String>,
    /// The replayable answer, once the work finished.
    pub answer: Option<String>,
    /// The session was deleted: the key is blocked until then.
    pub tombstone_until: Option<Timestamp>,
    /// When it was reserved.
    pub created_at: Timestamp,
}

/// What a reservation found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reserved {
    /// The key is new: this request owns it.
    Fresh(Admission),
    /// The key was reserved before.
    Existing(Admission),
}

/// A key to reserve.
#[derive(Debug, Clone, Copy)]
pub struct Key<'a> {
    /// The endpoint.
    pub endpoint: Endpoint,
    /// The bearer-key fingerprint.
    pub key_fp: &'a str,
    /// The `Idempotency-Key` header.
    pub idem_key: &'a str,
}

fn admission(row: &Row<'_>) -> rusqlite::Result<Admission> {
    Ok(Admission {
        id: row.get(0)?,
        identity: row.get(1)?,
        session_id: row.get(2)?,
        answer: row.get(3)?,
        tombstone_until: row.get::<_, Option<i64>>(4)?.map(timestamp),
        created_at: timestamp(row.get(5)?),
    })
}

const COLUMNS: &str = "id, identity, session_id, answer, tombstone_until, created_at";

fn find(tx: &Transaction<'_>, key: Key<'_>) -> Result<Option<Admission>, Error> {
    tx.query_row(
        &format!(
            "SELECT {COLUMNS} FROM api_admissions WHERE endpoint = ?1 AND key_fp = ?2 AND idem_key = ?3"
        ),
        params![key.endpoint.as_str(), key.key_fp, key.idem_key],
        admission,
    )
    .optional()
    .map_err(sql)
}

impl Store {
    /// Reserves `key` for work of `identity` in `session_id`, durably,
    /// unless it is reserved already.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn reserve_admission(
        &self,
        key: Key<'_>,
        identity: &str,
        session_id: Option<&str>,
    ) -> Result<Reserved, Error> {
        self.write(|tx| {
            if let Some(found) = find(tx, key)? {
                return Ok(Reserved::Existing(found));
            }
            let now = now_ms();
            let id = format!("adm_{}", ulid::Ulid::generate());
            tx.execute(
                "INSERT INTO api_admissions (id, endpoint, idem_key, key_fp, identity, session_id, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![id, key.endpoint.as_str(), key.idem_key, key.key_fp, identity, session_id, now],
            )
            .map_err(sql)?;
            Ok(Reserved::Fresh(Admission {
                id,
                identity: identity.to_owned(),
                session_id: session_id.map(str::to_owned),
                answer: None,
                tombstone_until: None,
                created_at: timestamp(now),
            }))
        })
    }

    /// Records the replayable answer of admission `id` and the session
    /// its work ran in.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn settle_admission(
        &self,
        id: &str,
        session_id: Option<&str>,
        answer: &str,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE api_admissions SET answer = ?2, session_id = coalesce(?3, session_id), \
                 updated_at = ?4 WHERE id = ?1 AND tombstone_until IS NULL",
                params![id, answer, session_id, now_ms()],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Drops admission `id` (its work was never admitted, so an exact
    /// retry is safe).
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn release_admission(&self, id: &str) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute("DELETE FROM api_admissions WHERE id = ?1", [id])
                .map(drop)
                .map_err(sql)
        })
    }

    /// Removes expired tombstones and finished admissions older than
    /// [`RETAIN_FOR`]; a run's key goes with its run's retention.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn prune_admissions(&self, now: Timestamp) -> Result<usize, Error> {
        let now = now.as_millisecond();
        let before = now - millis_of(RETAIN_FOR);
        self.write(|tx| {
            let mut removed = tx
                .execute(
                    "DELETE FROM api_admissions WHERE tombstone_until IS NOT NULL AND tombstone_until < ?1",
                    [now],
                )
                .map_err(sql)?;
            removed += tx
                .execute(
                    "DELETE FROM api_admissions WHERE tombstone_until IS NULL AND endpoint != 'runs' \
                     AND answer IS NOT NULL AND updated_at < ?1",
                    [before],
                )
                .map_err(sql)?;
            removed += tx
                .execute(
                    "DELETE FROM api_admissions WHERE tombstone_until IS NULL AND endpoint = 'runs' \
                     AND updated_at < ?1 AND NOT EXISTS (SELECT 1 FROM api_runs r \
                     WHERE r.admission_id = api_admissions.id AND r.updated_at >= ?1)",
                    [before],
                )
                .map_err(sql)?;
            Ok(removed)
        })
    }
}

/// Blocks the keys of a deleted session's admissions for
/// [`TOMBSTONE_FOR`] (inside the session's delete transaction).
pub(crate) fn tombstone(tx: &Transaction<'_>, session: &str) -> Result<(), Error> {
    let until = now_ms() + millis_of(TOMBSTONE_FOR);
    tx.execute(
        "UPDATE api_admissions SET answer = NULL, tombstone_until = ?2 WHERE session_id = ?1",
        params![session, until],
    )
    .map(drop)
    .map_err(sql)
}

/// `duration` in milliseconds.
pub(crate) fn millis_of(duration: SignedDuration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
