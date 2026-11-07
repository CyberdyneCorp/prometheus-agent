//! The per-profile tick lease (spec: scheduled-jobs, Built-in trigger
//! inside the gateway daemon; Headless tick without a daemon): one row, so
//! two processes never tick one profile at once. A lease is reclaimable
//! once it expired or when its holder is a dead process on this host.

use std::time::Duration;

use pmt_core::error::Error;
use pmt_core::runtime;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::sql;
use crate::leases::Holder;
use crate::sessions::now_ms;
use crate::store::Store;

/// Who holds the tick lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseHolder {
    /// The process.
    pub holder: Holder,
    /// `daemon` or `tick`.
    pub kind: String,
    /// When it lapses unless renewed, Unix ms.
    pub expires_at: i64,
}

/// A held tick lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickLease {
    token: String,
    ttl: Duration,
}

/// The result of an acquisition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronLease {
    /// The caller holds the lease.
    Granted(TickLease),
    /// Someone else does.
    Busy(LeaseHolder),
}

fn current(conn: &Connection) -> Result<Option<(String, LeaseHolder)>, Error> {
    conn.query_row(
        "SELECT token, kind, pid, process_start, host_id, expires_at FROM cron_lease WHERE id = 1",
        [],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                LeaseHolder {
                    kind: row.get(1)?,
                    holder: Holder {
                        pid: row.get(2)?,
                        process_start: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                        host_id: row.get(4)?,
                    },
                    expires_at: row.get(5)?,
                },
            ))
        },
    )
    .optional()
    .map_err(sql)
}

fn live(holder: &LeaseHolder) -> bool {
    holder.expires_at > now_ms() && !holder.holder.is_dead_local(&runtime::host_id())
}

fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

impl Store {
    /// Takes the tick lease for `holder` (`kind` is `daemon` or `tick`)
    /// for `ttl`, unless a live holder other than `holder` has it.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_acquire_lease(
        &self,
        holder: &Holder,
        kind: &str,
        ttl: Duration,
    ) -> Result<CronLease, Error> {
        self.write(|tx| {
            if let Some((_, current)) = current(tx)?
                && live(&current)
                && current.holder != *holder
            {
                return Ok(CronLease::Busy(current));
            }
            let token = ulid::Ulid::generate().to_string();
            let now = now_ms();
            tx.execute(
                "INSERT OR REPLACE INTO cron_lease (id, token, kind, pid, process_start, host_id, \
                 acquired_at, expires_at) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    token,
                    kind,
                    holder.pid,
                    i64::try_from(holder.process_start).unwrap_or(i64::MAX),
                    holder.host_id,
                    now,
                    now.saturating_add(ms(ttl))
                ],
            )
            .map_err(sql)?;
            Ok(CronLease::Granted(TickLease { token, ttl }))
        })
    }

    /// Extends `lease`; `false` when it was lost to another holder.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_renew_lease(&self, lease: &TickLease) -> Result<bool, Error> {
        self.write(|tx| {
            let n = tx
                .execute(
                    "UPDATE cron_lease SET expires_at = ?2 WHERE id = 1 AND token = ?1",
                    params![lease.token, now_ms().saturating_add(ms(lease.ttl))],
                )
                .map_err(sql)?;
            Ok(n > 0)
        })
    }

    /// Releases `lease` (left alone when someone else took it over).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_release_lease(&self, lease: &TickLease) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM cron_lease WHERE id = 1 AND token = ?1",
                params![lease.token],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// The live holder of the tick lease, if any.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_lease_holder(&self) -> Result<Option<LeaseHolder>, Error> {
        self.read(|conn| Ok(current(conn)?.map(|(_, h)| h).filter(live)))
    }
}
