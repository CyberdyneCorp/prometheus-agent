//! Provider credential request counts and benches (spec: model-providers,
//! Credential pool selection strategies; Exhausted credential cooldowns).
//!
//! One row per (provider, credential id). The id names a credential, never
//! its value (`secret:<NAME>`, `subscription:<label>`,
//! `borrowed:<cli>`). Every process of the profile shares the rows, so a
//! bench set by one session is honoured by every other one and survives a
//! restart.

use jiff::Timestamp;
use pmt_core::error::Error;
use rusqlite::params;

use crate::error::sql;
use crate::store::Store;

/// What the store knows about one credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialState {
    /// Credential id.
    pub credential: String,
    /// Requests it served.
    pub request_count: u64,
    /// When it last served one.
    pub last_used_at: Option<Timestamp>,
    /// Benched until then (may be in the past).
    pub benched_until: Option<Timestamp>,
    /// Why it was benched.
    pub bench_reason: Option<String>,
}

impl Store {
    /// Every recorded credential of `provider`, by id.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn credential_states(&self, provider: &str) -> Result<Vec<CredentialState>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT credential, request_count, last_used_at, benched_until, bench_reason \
                     FROM credential_state WHERE provider = ?1 ORDER BY credential",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map(params![provider], |row| {
                    Ok(CredentialState {
                        credential: row.get(0)?,
                        request_count: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                        last_used_at: millis(row.get(2)?),
                        benched_until: millis(row.get(3)?),
                        bench_reason: row.get(4)?,
                    })
                })
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })
    }

    /// Counts one request served by `credential` at `now`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn record_credential_use(
        &self,
        provider: &str,
        credential: &str,
        now: Timestamp,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO credential_state (provider, credential, request_count, last_used_at) \
                 VALUES (?1, ?2, 1, ?3) \
                 ON CONFLICT (provider, credential) DO UPDATE SET \
                 request_count = request_count + 1, last_used_at = excluded.last_used_at",
                params![provider, credential, now.as_millisecond()],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Benches `credential` until `until` (`None` clears the bench).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn bench_credential(
        &self,
        provider: &str,
        credential: &str,
        until: Option<Timestamp>,
        reason: Option<&str>,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO credential_state (provider, credential, benched_until, bench_reason) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (provider, credential) DO UPDATE SET \
                 benched_until = excluded.benched_until, bench_reason = excluded.bench_reason",
                params![
                    provider,
                    credential,
                    until.map(Timestamp::as_millisecond),
                    reason
                ],
            )
            .map_err(sql)?;
            Ok(())
        })
    }
}

fn millis(value: Option<i64>) -> Option<Timestamp> {
    value.and_then(|ms| Timestamp::from_millisecond(ms).ok())
}
