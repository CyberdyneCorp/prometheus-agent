//! The dispatcher lease (spec: kanban, Single dispatcher per home): one
//! row in each board's database names the dispatcher that may claim on
//! it. A lease lasts 60 s and its holder renews it every 20 s; another
//! dispatcher takes it over once it expired (or at once when the holder
//! is a dead process on this host), recording `dispatcher_takeover`.

use pmt_core::error::{Error, codes};
use rusqlite::{OptionalExtension as _, params};
use serde_json::json;

use crate::board::{Board, event};
use crate::db::sql;
use crate::model::Actor;
use crate::process;

/// How long a lease lasts without renewal.
pub const LEASE_MS: i64 = 60_000;
/// How often a holder renews.
pub const RENEW_MS: i64 = 20_000;

/// A dispatcher process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// `daemon` or `cli`.
    pub kind: String,
    /// Its pid.
    pub pid: u32,
    /// Its OS start time (Unix s).
    pub start: u64,
    /// Its host.
    pub host: String,
}

impl Holder {
    /// The calling process as a `kind` dispatcher.
    #[must_use]
    pub fn current(kind: &str) -> Self {
        let pid = std::process::id();
        Self {
            kind: kind.to_owned(),
            pid,
            start: pmt_core::runtime::process_start_time(pid).unwrap_or(0),
            host: pmt_core::runtime::host_id(),
        }
    }

    /// `daemon (pid 42 on host)`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} (pid {} on {})", self.kind, self.pid, self.host)
    }
}

/// The refusal while another live dispatcher holds a lease.
#[must_use]
pub fn held(holder: &str) -> Error {
    Error::new(
        &codes::KANBAN_LEASE_HELD,
        format!("dispatcher lease held by {holder}"),
    )
}

impl Board {
    /// Takes or renews this board's lease for `me`; `Ok(true)` when it
    /// was taken over from another dispatcher.
    ///
    /// # Errors
    ///
    /// `kanban.lease_held` while another live holder owns it.
    pub fn acquire_lease(&mut self, me: &Holder, now: i64) -> Result<bool, Error> {
        let actor = Actor::dispatcher().label();
        self.write(|tx| {
            let current: Option<(String, u32, i64, String, i64)> = tx
                .query_row(
                    "SELECT holder, pid, pid_start, host, expires_at FROM dispatcher_lease WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()
                .map_err(sql)?;
            let start = i64::try_from(me.start).unwrap_or(0);
            let takeover = match &current {
                None => false,
                Some((_, pid, pid_start, host, _)) if *pid == me.pid && *pid_start == start && *host == me.host => false,
                Some((holder, pid, pid_start, host, expires)) => {
                    let dead_here = *host == me.host
                        && !process::alive(*pid, u64::try_from(*pid_start).unwrap_or(0));
                    if *expires > now && !dead_here {
                        return Err(held(holder));
                    }
                    true
                }
            };
            let acquired_at = match &current {
                Some(_) if !takeover => None,
                _ => Some(now),
            };
            tx.execute(
                "INSERT INTO dispatcher_lease(id, holder, pid, pid_start, host, expires_at, acquired_at) \
                 VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id) DO UPDATE SET holder = ?1, pid = ?2, \
                 pid_start = ?3, host = ?4, expires_at = ?5, acquired_at = COALESCE(?7, acquired_at)",
                params![me.label(), me.pid, start, me.host, now + LEASE_MS, now, acquired_at],
            )
            .map_err(sql)?;
            if takeover {
                let previous = current.map(|c| c.0);
                event(tx, None, None, &actor, "dispatcher_takeover", &json!({"previous": previous, "holder": me.label()}), now)?;
            }
            Ok(takeover)
        })
    }

    /// Gives the lease up when `me` holds it.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn release_lease(&mut self, me: &Holder) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM dispatcher_lease WHERE id = 1 AND pid = ?1 AND host = ?2",
                params![me.pid, me.host],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// The current holder's label and expiry, if any.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn lease(&self) -> Result<Option<(String, i64)>, Error> {
        self.conn()
            .query_row(
                "SELECT holder, expires_at FROM dispatcher_lease WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql)
    }
}
