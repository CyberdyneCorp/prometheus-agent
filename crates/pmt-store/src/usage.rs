//! The per-call usage ledger and its totals (spec: session-storage, Usage
//! events keyed by root lineage; observability, Per-call usage ledger and
//! Totals keyed by root lineage).
//!
//! Every row carries the `root_session_id` of its session at insert time,
//! so a lineage total (continuations, forks, delegated children) is one
//! sum over the root. Rows are append-only (an UPDATE trigger refuses
//! changes); only session deletion removes them. Pricing is the caller's
//! (observability): the store records what it is given.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{Connection, params};

use crate::error::sql;
use crate::model::CostStatus;
use crate::sessions;
use crate::store::Store;

/// Token counts by class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Input (prompt) tokens.
    pub input: u64,
    /// Output (completion) tokens.
    pub output: u64,
    /// Tokens read from the provider's prompt cache.
    pub cache_read: u64,
    /// Tokens written to the provider's prompt cache.
    pub cache_write: u64,
    /// Reasoning tokens.
    pub reasoning: u64,
}

/// One model or metered tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEvent {
    /// Unique call id.
    pub call_id: String,
    /// When the call finished.
    pub ts: Timestamp,
    /// The session that made the call.
    pub session_id: SessionId,
    /// The turn, when inside one.
    pub turn_id: Option<String>,
    /// The call this one serves (`MoA` references, retries).
    pub parent_call_id: Option<String>,
    /// Dotted purpose: `main`, `auxiliary.title`, `tool.image_generate`...
    pub purpose: String,
    /// Extra purpose detail (for example a `MoA` preset name).
    pub purpose_detail: Option<String>,
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// Provider service tier billed.
    pub service_tier: Option<String>,
    /// Tokens by class.
    pub tokens: Tokens,
    /// Cost, when priced.
    pub cost_usd: Option<f64>,
    /// How the cost is known.
    pub cost_status: CostStatus,
    /// `subscription:<provider>[/<label>]`, `api_key:<provider>[/<label>]`,
    /// `keyless:<vendor>` or `local`.
    pub billing_source: String,
    /// Hash of the request (replay cache key).
    pub request_hash: Option<String>,
    /// Served from the replay cache.
    pub cache_hit: bool,
}

/// Which rows a total sums.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageScope {
    /// One session's own calls.
    Session(SessionId),
    /// Every call of the lineage the session belongs to (by its root).
    Lineage(SessionId),
    /// Every call of the profile.
    Profile,
}

/// Summed usage.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    /// Calls summed.
    pub calls: u64,
    /// Tokens by class.
    pub tokens: Tokens,
    /// Sum of the priced calls' cost.
    pub cost_usd: f64,
    /// Calls without a price (`cost_usd` null).
    pub unpriced_calls: u64,
}

fn count(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn totals(conn: &Connection, filter: &str, param: Option<String>) -> Result<UsageTotals, Error> {
    let sql_text = format!(
        "SELECT count(*), COALESCE(sum(input_tokens), 0), COALESCE(sum(output_tokens), 0), \
         COALESCE(sum(cache_read_tokens), 0), COALESCE(sum(cache_write_tokens), 0), \
         COALESCE(sum(reasoning_tokens), 0), COALESCE(sum(cost_usd), 0.0), \
         count(*) - count(cost_usd) FROM usage_ledger {filter}"
    );
    let read = |row: &rusqlite::Row<'_>| -> rusqlite::Result<UsageTotals> {
        let n = |i: usize| -> rusqlite::Result<u64> {
            Ok(u64::try_from(row.get::<_, i64>(i)?).unwrap_or(0))
        };
        Ok(UsageTotals {
            calls: n(0)?,
            tokens: Tokens {
                input: n(1)?,
                output: n(2)?,
                cache_read: n(3)?,
                cache_write: n(4)?,
                reasoning: n(5)?,
            },
            cost_usd: row.get(6)?,
            unpriced_calls: n(7)?,
        })
    };
    match param {
        Some(param) => conn.query_row(&sql_text, [param], read),
        None => conn.query_row(&sql_text, [], read),
    }
    .map_err(sql)
}

fn event_from_row(row: &rusqlite::Row<'_>) -> Result<UsageEvent, Error> {
    let n = |i: usize| -> Result<u64, Error> {
        Ok(u64::try_from(row.get::<_, i64>(i).map_err(sql)?).unwrap_or(0))
    };
    let text = |i: usize| row.get::<_, Option<String>>(i).map_err(sql);
    Ok(UsageEvent {
        call_id: row.get(0).map_err(sql)?,
        ts: Timestamp::from_millisecond(row.get(1).map_err(sql)?).unwrap_or(Timestamp::UNIX_EPOCH),
        session_id: SessionId::parse(&row.get::<_, String>(2).map_err(sql)?)?,
        turn_id: text(3)?,
        parent_call_id: text(4)?,
        purpose: row.get(5).map_err(sql)?,
        purpose_detail: text(6)?,
        provider: row.get(7).map_err(sql)?,
        model: row.get(8).map_err(sql)?,
        service_tier: text(9)?,
        tokens: Tokens {
            input: n(10)?,
            output: n(11)?,
            cache_read: n(12)?,
            cache_write: n(13)?,
            reasoning: n(14)?,
        },
        cost_usd: row.get(15).map_err(sql)?,
        cost_status: row.get::<_, String>(16).map_err(sql)?.parse()?,
        billing_source: row.get(17).map_err(sql)?,
        request_hash: text(18)?,
        cache_hit: row.get(19).map_err(sql)?,
    })
}

impl Store {
    /// A session's own ledger rows, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn usage_events(&self, session: SessionId) -> Result<Vec<UsageEvent>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT call_id, ts, session_id, turn_id, parent_call_id, purpose, \
                     purpose_detail, provider, model, service_tier, input_tokens, output_tokens, \
                     cache_read_tokens, cache_write_tokens, reasoning_tokens, cost_usd, \
                     cost_status, billing_source, request_hash, cache_hit FROM usage_ledger \
                     WHERE session_id = ?1 ORDER BY ts, call_id",
                )
                .map_err(sql)?;
            let mut rows = stmt.query([session.to_string()]).map_err(sql)?;
            let mut events = Vec::new();
            while let Some(row) = rows.next().map_err(sql)? {
                events.push(event_from_row(row)?);
            }
            Ok(events)
        })
    }

    /// Appends one ledger row; its `root_session_id` is the session's
    /// root.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.constraint` (duplicate call id, a
    /// malformed billing source, negative tokens), `store.busy`,
    /// `store.sqlite`.
    pub fn record_usage(&self, event: &UsageEvent) -> Result<(), Error> {
        let profile = self.profile_name().to_owned();
        self.write(|tx| {
            let session = sessions::load(tx, event.session_id)?
                .ok_or_else(|| sessions::not_found(event.session_id))?;
            tx.execute(
                "INSERT INTO usage_ledger (call_id, ts, profile, session_id, root_session_id, \
                 turn_id, parent_call_id, purpose, purpose_detail, provider, model, service_tier, \
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
                 reasoning_tokens, cost_usd, cost_status, billing_source, request_hash, cache_hit) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                 ?17, ?18, ?19, ?20, ?21, ?22)",
                params![
                    event.call_id,
                    event.ts.as_millisecond(),
                    profile,
                    event.session_id.to_string(),
                    session.root_session_id.to_string(),
                    event.turn_id,
                    event.parent_call_id,
                    event.purpose,
                    event.purpose_detail,
                    event.provider,
                    event.model,
                    event.service_tier,
                    count(event.tokens.input),
                    count(event.tokens.output),
                    count(event.tokens.cache_read),
                    count(event.tokens.cache_write),
                    count(event.tokens.reasoning),
                    event.cost_usd,
                    event.cost_status.as_str(),
                    event.billing_source,
                    event.request_hash,
                    event.cache_hit,
                ],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Sums the ledger rows of `scope`.
    ///
    /// # Errors
    ///
    /// `session.not_found` (session and lineage scopes), `store.sqlite`.
    pub fn usage_totals(&self, scope: UsageScope) -> Result<UsageTotals, Error> {
        self.read(|conn| match scope {
            UsageScope::Profile => totals(conn, "", None),
            UsageScope::Session(id) => {
                sessions::load(conn, id)?.ok_or_else(|| sessions::not_found(id))?;
                totals(conn, "WHERE session_id = ?1", Some(id.to_string()))
            }
            UsageScope::Lineage(id) => {
                let session = sessions::load(conn, id)?.ok_or_else(|| sessions::not_found(id))?;
                totals(
                    conn,
                    "WHERE root_session_id = ?1",
                    Some(session.root_session_id.to_string()),
                )
            }
        })
    }
}
