//! The usage ledger's writer, pricing and reports (spec: observability,
//! Per-call usage ledger, Pricing and unpriced calls, Totals keyed by root
//! lineage, Usage command).
//!
//! The ledger itself is the `usage_ledger` table of `state.db`
//! (`pmt_store::usage`); this module prices each call before it is
//! appended ([`UsageRecorder`]) and sums rows for reports ([`report`]).
//! Unpriced calls are shown as `—` ([`format_cost`]), never `$0.00`.

mod price;
pub mod report;

pub use price::{Billing, ConfigPrices, Price, Prices, price_call};

use std::sync::Arc;

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_store::{Store, Tokens, UsageEvent, UsageTotals};

/// One finished model call or metered tool call, as its caller knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// The call id (also on the trajectory's `model.call`); `None` makes a
    /// fresh `call_<ulid>`.
    pub call_id: Option<String>,
    /// The session that made the call.
    pub session_id: SessionId,
    /// The turn, when inside one.
    pub turn_id: Option<String>,
    /// The call this one serves (`MoA` references, retries).
    pub parent_call_id: Option<String>,
    /// Dotted purpose: `main`, `auxiliary.title`, `compression`,
    /// `moa.reference`, `tool.image_generate`...
    pub purpose: String,
    /// Extra purpose detail (a `MoA` preset name).
    pub purpose_detail: Option<String>,
    /// Provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Provider service tier billed (`priority`, `flex`).
    pub service_tier: Option<String>,
    /// Tokens by class (reasoning tokens are part of the output tokens).
    pub tokens: Tokens,
    /// Cost the provider reported, when it reports one.
    pub reported_usd: Option<f64>,
    /// `subscription:<provider>[/<label>]`, `api_key:<provider>[/<label>]`,
    /// `keyless:<vendor>` or `local`.
    pub billing_source: String,
    /// Hash of the canonical request.
    pub request_hash: Option<String>,
    /// Served from the replay cache.
    pub cache_hit: bool,
}

impl Call {
    /// A call with no tokens yet; fill the rest with struct update syntax.
    #[must_use]
    pub fn new(session_id: SessionId, purpose: &str, provider: &str, model: &str) -> Self {
        Self {
            call_id: None,
            session_id,
            turn_id: None,
            parent_call_id: None,
            purpose: purpose.to_owned(),
            purpose_detail: None,
            provider: provider.to_owned(),
            model: model.to_owned(),
            service_tier: None,
            tokens: Tokens::default(),
            reported_usd: None,
            billing_source: "local".to_owned(),
            request_hash: None,
            cache_hit: false,
        }
    }
}

/// Prices calls and appends exactly one ledger row per call. One per
/// profile (share it: the store is `Send + Sync`).
pub struct UsageRecorder {
    store: Arc<Store>,
    prices: Arc<dyn Prices>,
}

impl std::fmt::Debug for UsageRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageRecorder").finish_non_exhaustive()
    }
}

impl UsageRecorder {
    /// A recorder writing to `store` with `prices`.
    #[must_use]
    pub fn new(store: Arc<Store>, prices: Arc<dyn Prices>) -> Self {
        Self { store, prices }
    }

    /// The price source (budget estimates use the same one).
    #[must_use]
    pub fn prices(&self) -> &dyn Prices {
        self.prices.as_ref()
    }

    /// Prices `call` and appends its ledger row (finished now); returns
    /// the row, whose `cost_usd`/`cost_status` the trajectory's
    /// `model.call` should repeat.
    ///
    /// # Errors
    ///
    /// The store's `record_usage` errors (`session.not_found`,
    /// `store.constraint` for a duplicate call id or a malformed billing
    /// source, `store.busy`).
    pub fn record(&self, call: Call) -> Result<UsageEvent, Error> {
        self.record_at(call, Timestamp::now())
    }

    /// [`record`](Self::record) with an explicit finish time.
    ///
    /// # Errors
    ///
    /// As [`record`](Self::record).
    pub fn record_at(&self, call: Call, ts: Timestamp) -> Result<UsageEvent, Error> {
        let price = self.prices.price(&call.provider, &call.model);
        let (cost_usd, cost_status) = price_call(
            &Billing {
                tokens: &call.tokens,
                reported_usd: call.reported_usd,
                billing_source: &call.billing_source,
                cache_hit: call.cache_hit,
            },
            price.as_ref(),
        );
        let event = UsageEvent {
            call_id: call.call_id.unwrap_or_else(|| {
                format!("call_{}", ulid::Ulid::generate().to_string().to_lowercase())
            }),
            ts,
            session_id: call.session_id,
            turn_id: call.turn_id,
            parent_call_id: call.parent_call_id,
            purpose: call.purpose,
            purpose_detail: call.purpose_detail,
            provider: call.provider,
            model: call.model,
            service_tier: call.service_tier,
            tokens: call.tokens,
            cost_usd,
            cost_status,
            billing_source: call.billing_source,
            request_hash: call.request_hash,
            cache_hit: call.cache_hit,
        };
        self.store.record_usage(&event)?;
        Ok(event)
    }
}

/// One call's cost: `$0.0123`, or `—` when unpriced.
#[must_use]
pub fn format_cost(cost_usd: Option<f64>) -> String {
    cost_usd.map_or_else(|| "—".to_owned(), |cost| format!("${cost:.4}"))
}

/// A total's cost: the priced calls' sum, or `—` when every call was
/// unpriced (an unpriced call never counts as `$0.00`; the
/// `unpriced_calls` count is shown beside it).
#[must_use]
pub fn format_total_cost(totals: &UsageTotals) -> String {
    if totals.calls > 0 && totals.unpriced_calls == totals.calls {
        format_cost(None)
    } else {
        format_cost(Some(totals.cost_usd))
    }
}
