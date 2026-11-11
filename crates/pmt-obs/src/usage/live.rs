//! Live usage events (spec: observability, Live usage events): every
//! ledger row a recorder appends is handed to the process's listeners (the
//! daemon turns it into `usage.updated` for the session's clients), and
//! [`payload`] builds that event from the ledger so every client shows the
//! same numbers.

use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use jiff::Timestamp;
use pmt_config::schema::BudgetsConfig;
use pmt_core::error::Error;
use pmt_store::{Store, UsageEvent, UsageScope, UsageTotals};
use serde_json::{Value, json};

use crate::budget::{Budgets, Place};

type Listener = Arc<dyn Fn(&str, &UsageEvent) + Send + Sync>;

fn listeners() -> &'static RwLock<Vec<Listener>> {
    static LISTENERS: OnceLock<RwLock<Vec<Listener>>> = OnceLock::new();
    LISTENERS.get_or_init(RwLock::default)
}

/// Calls `listener` with the profile and row of every ledger row appended
/// from now on, on the recording thread (keep it short: spawn the work).
pub fn on_recorded(listener: impl Fn(&str, &UsageEvent) + Send + Sync + 'static) {
    listeners()
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Arc::new(listener));
}

pub(super) fn notify(profile: &str, row: &UsageEvent) {
    let listeners = listeners()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    for listener in listeners {
        listener(profile, row);
    }
}

/// The `usage.updated` payload of `row`: the row (without its request
/// hash), the session's and the lineage's totals from the ledger, and
/// every configured budget with its spend (`zone` evaluates days and
/// months).
///
/// # Errors
///
/// Store errors (`session.not_found`, `store.sqlite`).
pub fn payload(
    store: &Store,
    row: &UsageEvent,
    budgets: &BudgetsConfig,
    zone: jiff::tz::TimeZone,
) -> Result<Value, Error> {
    let place = Place::of(store, row.session_id, None)?;
    let session_totals = store.usage_totals(UsageScope::Session(row.session_id))?;
    let root_totals = store.usage_totals(UsageScope::Lineage(row.session_id))?;
    let standing: Vec<Value> = Budgets::new(zone)
        .status(store, budgets, &place, false, Timestamp::now())?
        .into_iter()
        .map(|(limit, spent)| {
            json!({
                "scope": limit.scope.as_str(),
                "unit": limit.unit.as_str(),
                "limit": limit.limit,
                "spent": spent,
                "remaining": (limit.limit - spent).max(0.0),
            })
        })
        .collect();
    Ok(json!({
        "session_id": row.session_id.to_string(),
        "root_session_id": place.root.to_string(),
        "call": call(row),
        "session_totals": totals(&session_totals),
        "root_totals": totals(&root_totals),
        "budgets": standing,
    }))
}

/// The ledger row without `request_hash`.
fn call(row: &UsageEvent) -> Value {
    json!({
        "call_id": row.call_id,
        "ts": crate::trajectory::timestamp(row.ts),
        "session_id": row.session_id.to_string(),
        "turn_id": row.turn_id,
        "parent_call_id": row.parent_call_id,
        "purpose": row.purpose,
        "purpose_detail": row.purpose_detail,
        "provider": row.provider,
        "model": row.model,
        "service_tier": row.service_tier,
        "input_tokens": row.tokens.input,
        "output_tokens": row.tokens.output,
        "cache_read_tokens": row.tokens.cache_read,
        "cache_write_tokens": row.tokens.cache_write,
        "reasoning_tokens": row.tokens.reasoning,
        "cost_usd": row.cost_usd,
        "cost_status": row.cost_status.as_str(),
        "billing_source": row.billing_source,
        "cache_hit": row.cache_hit,
    })
}

fn totals(totals: &UsageTotals) -> Value {
    json!({
        "calls": totals.calls,
        "input_tokens": totals.tokens.input,
        "output_tokens": totals.tokens.output,
        "cache_read_tokens": totals.tokens.cache_read,
        "cache_write_tokens": totals.tokens.cache_write,
        "reasoning_tokens": totals.tokens.reasoning,
        "cost_usd": totals.cost_usd,
        "unpriced_calls": totals.unpriced_calls,
    })
}
