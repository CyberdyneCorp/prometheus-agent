//! Turn leases and compaction locks (spec: session-storage, Turn lease and
//! compaction lock; context-compression, One compaction at a time per
//! session).
//!
//! A lease is a row keyed by (session, kind), so at most one of each kind
//! is live per session across every process sharing the store. A lease is
//! reclaimable once it expired or when its holder is a process on this
//! host that no longer runs (pid gone, or reused by a process with another
//! start time); a holder on another host is trusted until expiry. A held
//! lease is reported as [`Acquire::Busy`], not as an error.

use std::time::Duration;

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_core::runtime;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::sql;
use crate::sessions::{self, now_ms};
use crate::store::Store;

/// How often a turn holder renews its lease.
pub const TURN_LEASE_RENEW_EVERY: Duration = Duration::from_secs(30);

/// The two per-session exclusive leases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaseKind {
    /// A running turn.
    Turn,
    /// A running compaction.
    Compaction,
}

impl LeaseKind {
    /// Stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Compaction => "compaction",
        }
    }

    /// Default lifetime: 120 s for a turn (renewed every 30 s), 300 s for
    /// a compaction.
    #[must_use]
    pub const fn default_ttl(self) -> Duration {
        match self {
            Self::Turn => Duration::from_mins(2),
            Self::Compaction => Duration::from_mins(5),
        }
    }
}

/// Who holds a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// OS process id.
    pub pid: u32,
    /// OS start time of `pid`, Unix seconds.
    pub process_start: u64,
    /// The holder's host ([`runtime::host_id`]).
    pub host_id: String,
}

impl Holder {
    /// The calling process.
    #[must_use]
    pub fn current() -> Self {
        let pid = std::process::id();
        Self {
            pid,
            process_start: runtime::process_start_time(pid).unwrap_or(0),
            host_id: runtime::host_id(),
        }
    }

    /// Whether the holder is a process on this host that no longer runs.
    fn is_dead_local(&self, this_host: &str) -> bool {
        !this_host.is_empty()
            && self.host_id == this_host
            && runtime::process_start_time(self.pid) != Some(self.process_start)
    }
}

/// A held lease; pass it to [`Store::renew_lease`],
/// [`Store::release_lease`] or the operation it guards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    session: SessionId,
    kind: LeaseKind,
    token: String,
    ttl: Duration,
    expires_at: Timestamp,
}

impl Lease {
    /// The leased session.
    #[must_use]
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Turn or compaction.
    #[must_use]
    pub fn kind(&self) -> LeaseKind {
        self.kind
    }

    /// When it lapses unless renewed.
    #[must_use]
    pub fn expires_at(&self) -> Timestamp {
        self.expires_at
    }
}

/// The result of an acquisition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquire {
    /// The caller now holds the lease.
    Granted(Lease),
    /// Someone else holds it.
    Busy {
        /// The current holder.
        holder: Holder,
        /// When its lease lapses unless renewed.
        expires_at: Timestamp,
    },
}

struct Row {
    token: String,
    holder: Holder,
    expires_at: i64,
}

fn current(conn: &Connection, session: SessionId, kind: LeaseKind) -> Result<Option<Row>, Error> {
    conn.query_row(
        "SELECT token, pid, process_start, host_id, expires_at FROM leases \
         WHERE session_id = ?1 AND kind = ?2",
        params![session.to_string(), kind.as_str()],
        |row| {
            Ok(Row {
                token: row.get(0)?,
                holder: Holder {
                    pid: row.get(1)?,
                    process_start: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                    host_id: row.get(3)?,
                },
                expires_at: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(sql)
}

fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn timestamp(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH)
}

fn lost(lease: &Lease) -> Error {
    Error::new(
        &codes::STORE_LEASE_LOST,
        format!(
            "the {} lease on session {} is no longer held by this caller",
            lease.kind.as_str(),
            lease.session
        ),
    )
}

/// Fails unless `lease` is still the live lease of its session.
pub(crate) fn check_held(conn: &Connection, lease: &Lease) -> Result<(), Error> {
    match current(conn, lease.session, lease.kind)? {
        Some(row) if row.token == lease.token && row.expires_at > now_ms() => Ok(()),
        _ => Err(lost(lease)),
    }
}

impl Store {
    /// Acquires `kind` on `session` for `holder` with the default lifetime.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.schema_too_new`, `store.sqlite`.
    pub fn acquire_lease(
        &self,
        session: SessionId,
        kind: LeaseKind,
        holder: &Holder,
    ) -> Result<Acquire, Error> {
        self.acquire_lease_for(session, kind, holder, kind.default_ttl())
    }

    /// Acquires `kind` on `session` for `ttl`, reclaiming an expired lease
    /// or one whose local holder process is dead.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.schema_too_new`, `store.sqlite`.
    pub fn acquire_lease_for(
        &self,
        session: SessionId,
        kind: LeaseKind,
        holder: &Holder,
        ttl: Duration,
    ) -> Result<Acquire, Error> {
        let this_host = runtime::host_id();
        self.write(|tx| {
            if sessions::load(tx, session)?.is_none() {
                return Err(sessions::not_found(session));
            }
            let now = now_ms();
            if let Some(row) = current(tx, session, kind)?
                && row.expires_at > now
                && !row.holder.is_dead_local(&this_host)
            {
                return Ok(Acquire::Busy {
                    holder: row.holder,
                    expires_at: timestamp(row.expires_at),
                });
            }
            let token = ulid::Ulid::generate().to_string();
            let expires = now.saturating_add(ms(ttl));
            tx.execute(
                "INSERT OR REPLACE INTO leases (session_id, kind, token, pid, process_start, \
                 host_id, acquired_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    session.to_string(),
                    kind.as_str(),
                    token,
                    holder.pid,
                    i64::try_from(holder.process_start).unwrap_or(i64::MAX),
                    holder.host_id,
                    now,
                    expires
                ],
            )
            .map_err(sql)?;
            Ok(Acquire::Granted(Lease {
                session,
                kind,
                token,
                ttl,
                expires_at: timestamp(expires),
            }))
        })
    }

    /// Extends `lease` by its lifetime from now.
    ///
    /// # Errors
    ///
    /// `store.lease_lost` when it expired and was reclaimed (or released),
    /// `store.schema_too_new`, `store.sqlite`.
    pub fn renew_lease(&self, lease: &mut Lease) -> Result<(), Error> {
        let expires = self.write(|tx| {
            check_held(tx, lease)?;
            let expires = now_ms().saturating_add(ms(lease.ttl));
            tx.execute(
                "UPDATE leases SET expires_at = ?3 WHERE session_id = ?1 AND kind = ?2",
                params![lease.session.to_string(), lease.kind.as_str(), expires],
            )
            .map_err(sql)?;
            Ok(expires)
        })?;
        lease.expires_at = timestamp(expires);
        Ok(())
    }

    /// Releases `lease`; a lease that was already reclaimed is left to its
    /// new holder.
    ///
    /// # Errors
    ///
    /// `store.schema_too_new`, `store.sqlite`.
    pub fn release_lease(&self, lease: &Lease) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM leases WHERE session_id = ?1 AND kind = ?2 AND token = ?3",
                params![lease.session.to_string(), lease.kind.as_str(), lease.token],
            )
            .map(drop)
            .map_err(sql)
        })
    }
}
