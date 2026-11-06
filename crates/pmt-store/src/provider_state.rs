//! Per-provider routing state (spec: model-providers, Provider health
//! scoring; Subscription plan and quota visibility; Provider spend caps):
//! call outcomes health scores are computed over, the newest quota probe
//! of each subscription credential, what a provider spent since a time,
//! and which spend-cap periods already warned. Every process of the
//! profile shares the rows.

use jiff::Timestamp;
use pmt_core::error::Error;
use rusqlite::{OptionalExtension, params};

use crate::error::sql;
use crate::store::Store;

/// Rows kept per (provider, model, credential).
const KEPT_CALLS: i64 = 500;

/// One model call outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct CallSample {
    /// Provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Credential id (`secret:<NAME>`, `subscription:<label>`, `none`).
    pub credential: String,
    /// When the call ended.
    pub ts: Timestamp,
    /// `None` on success, else the error class.
    pub class: Option<String>,
    /// Time to the first streamed event.
    pub ttft_ms: Option<u64>,
    /// Output tokens per second after the first event.
    pub output_tps: Option<f64>,
}

/// The newest usage probe of one subscription credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaRow {
    /// When it was fetched.
    pub fetched_at: Timestamp,
    /// The plan label, when reported.
    pub plan: Option<String>,
    /// JSON array of the quota windows.
    pub windows: String,
}

impl Store {
    /// Records one call outcome, keeping the newest 500 of its key.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn record_provider_call(&self, sample: &CallSample) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO provider_calls \
                 (provider, model, credential, ts, ok, class, ttft_ms, output_tps) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    sample.provider,
                    sample.model,
                    sample.credential,
                    sample.ts.as_millisecond(),
                    i64::from(sample.class.is_none()),
                    sample.class,
                    sample.ttft_ms.and_then(|ms| i64::try_from(ms).ok()),
                    sample.output_tps,
                ],
            )
            .map_err(sql)?;
            tx.execute(
                "DELETE FROM provider_calls WHERE provider = ?1 AND model = ?2 AND credential = ?3 \
                 AND id NOT IN (SELECT id FROM provider_calls WHERE provider = ?1 AND model = ?2 \
                 AND credential = ?3 ORDER BY id DESC LIMIT ?4)",
                params![sample.provider, sample.model, sample.credential, KEPT_CALLS],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// The newest `limit` outcomes of (`provider`, `model`, `credential`)
    /// (every credential when `None`), newest first.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn provider_calls(
        &self,
        provider: &str,
        model: &str,
        credential: Option<&str>,
        limit: u32,
    ) -> Result<Vec<CallSample>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT ts, class, ttft_ms, output_tps, credential FROM provider_calls \
                     WHERE provider = ?1 AND model = ?2 AND (?3 IS NULL OR credential = ?3) \
                     ORDER BY id DESC LIMIT ?4",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map(params![provider, model, credential, limit], |row| {
                    Ok(CallSample {
                        provider: provider.to_owned(),
                        model: model.to_owned(),
                        credential: row.get(4)?,
                        ts: Timestamp::from_millisecond(row.get(0)?).unwrap_or_default(),
                        class: row.get(1)?,
                        ttft_ms: row
                            .get::<_, Option<i64>>(2)?
                            .and_then(|ms| u64::try_from(ms).ok()),
                        output_tps: row.get(3)?,
                    })
                })
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })
    }

    /// Every (provider, model, credential) with recorded outcomes.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn provider_call_keys(&self) -> Result<Vec<(String, String, String)>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT DISTINCT provider, model, credential FROM provider_calls \
                     ORDER BY provider, model, credential",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })
    }

    /// Stores the newest quota probe of `credential` of `provider`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn put_subscription_quota(
        &self,
        provider: &str,
        credential: &str,
        row: &QuotaRow,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO subscription_quota (provider, credential, fetched_at, plan, windows) \
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (provider, credential) DO UPDATE SET \
                 fetched_at = excluded.fetched_at, plan = excluded.plan, windows = excluded.windows",
                params![
                    provider,
                    credential,
                    row.fetched_at.as_millisecond(),
                    row.plan,
                    row.windows
                ],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// The newest quota probe of `credential` of `provider`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn subscription_quota(
        &self,
        provider: &str,
        credential: &str,
    ) -> Result<Option<QuotaRow>, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT fetched_at, plan, windows FROM subscription_quota \
                 WHERE provider = ?1 AND credential = ?2",
                params![provider, credential],
                |row| {
                    Ok(QuotaRow {
                        fetched_at: Timestamp::from_millisecond(row.get(0)?).unwrap_or_default(),
                        plan: row.get(1)?,
                        windows: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(sql)
        })
    }

    /// What the profile's ledger records as spent on `provider` since
    /// `since` (calls without a known cost count zero).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn provider_spent_since(&self, provider: &str, since: Timestamp) -> Result<f64, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT coalesce(sum(cost_usd), 0.0) FROM usage_ledger \
                 WHERE provider = ?1 AND ts >= ?2",
                params![provider, since.as_millisecond()],
                |row| row.get::<_, f64>(0),
            )
            .map_err(sql)
        })
    }

    /// Marks `period` of `provider` as warned; `true` the first time.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn mark_budget_warning(
        &self,
        provider: &str,
        period: &str,
        now: Timestamp,
    ) -> Result<bool, Error> {
        self.write(|tx| {
            let inserted = tx
                .execute(
                    "INSERT OR IGNORE INTO provider_budget_warnings (provider, period, warned_at) \
                     VALUES (?1, ?2, ?3)",
                    params![provider, period, now.as_millisecond()],
                )
                .map_err(sql)?;
            Ok(inserted == 1)
        })
    }

    /// Records that API key `credential` of `provider` came from `source`
    /// (and is usable).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn set_key_origin(
        &self,
        provider: &str,
        credential: &str,
        source: &str,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO key_origins (provider, credential, source, dead) VALUES (?1, ?2, ?3, 0) \
                 ON CONFLICT (provider, credential) DO UPDATE SET source = excluded.source, dead = 0",
                params![provider, credential, source],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Where API key `credential` of `provider` came from and whether it
    /// is dead; `None` for a key stored by hand.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn key_origin(
        &self,
        provider: &str,
        credential: &str,
    ) -> Result<Option<(String, bool)>, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT source, dead FROM key_origins WHERE provider = ?1 AND credential = ?2",
                params![provider, credential],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .optional()
            .map_err(sql)
        })
    }

    /// Marks minted key `credential` of `provider` dead.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn mark_key_dead(&self, provider: &str, credential: &str) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE key_origins SET dead = 1 WHERE provider = ?1 AND credential = ?2",
                params![provider, credential],
            )
            .map_err(sql)?;
            Ok(())
        })
    }
}
