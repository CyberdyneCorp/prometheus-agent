//! The observability methods and events (spec: observability, Metrics
//! catalog; Component health model; Live usage events): `system.metrics`,
//! `system.health`, `usage.updated` and `health.changed`.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Event, Method};

/// `system.metrics` and `system.health` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SystemParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
}

/// One data point of a metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricPoint {
    /// Its bounded attributes (never content, paths or user ids).
    pub attributes: BTreeMap<String, String>,
    /// A counter's or gauge's value; a histogram's observation count.
    pub value: f64,
    /// A histogram's sum of observations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sum: Option<f64>,
    /// A histogram's per-bucket counts (the bounds of `bounds`, then the
    /// overflow bucket).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buckets: Option<Vec<u64>>,
}

/// One metric of the catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricView {
    /// `prometheus.tokens`...
    pub name: String,
    /// `counter`, `histogram` or `gauge`.
    pub kind: String,
    /// The UCUM unit.
    pub unit: String,
    /// What it counts.
    pub description: String,
    /// Its data points.
    pub points: Vec<MetricPoint>,
}

/// `system.metrics` result: the metrics catalog now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricsSnapshot {
    /// Upper bucket bounds of every histogram, in milliseconds.
    pub bounds: Vec<f64>,
    /// Every catalog metric.
    pub metrics: Vec<MetricView>,
}

/// One component's health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComponentHealth {
    /// `store`, `scheduler`, `adapter:<platform>`, `mcp:<server>`...
    pub component: String,
    /// The owning profile; `null` for daemon-wide components.
    #[serde(default)]
    pub profile: Option<String>,
    /// `ok`, `degraded` or `down`.
    pub state: String,
    /// Why.
    pub reason: String,
    /// When it entered the state (RFC 3339).
    pub since: String,
}

/// `system.health` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SystemHealth {
    /// The profile's components and the daemon-wide ones.
    pub components: Vec<ComponentHealth>,
}

/// `health.changed` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HealthChanged {
    /// The component.
    pub component: String,
    /// Its profile; `null` for daemon-wide components.
    #[serde(default)]
    pub profile: Option<String>,
    /// The new state.
    pub state: String,
    /// The previous state (`null` when first reported).
    #[serde(default)]
    pub previous: Option<String>,
    /// Why.
    pub reason: String,
    /// When (RFC 3339).
    pub since: String,
}

/// One budget's standing in `usage.updated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetStanding {
    /// `turn`, `session`, `root`, `daily`, `monthly` or `unattended`.
    pub scope: String,
    /// `usd` or `tokens`.
    pub unit: String,
    /// The limit.
    pub limit: f64,
    /// Spent in the scope's period.
    pub spent: f64,
    /// `limit - spent`, never below zero.
    pub remaining: f64,
}

/// Usage totals of a session or a lineage.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsageTotalsView {
    /// Calls.
    pub calls: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cache-read tokens.
    pub cache_read_tokens: u64,
    /// Cache-write tokens.
    pub cache_write_tokens: u64,
    /// Reasoning tokens.
    pub reasoning_tokens: u64,
    /// Sum of the priced calls.
    pub cost_usd: f64,
    /// Calls without a price.
    pub unpriced_calls: u64,
}

/// `usage.updated` payload: after every model call, the same numbers for
/// every client attached to the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsageUpdated {
    /// The session that made the call.
    pub session_id: String,
    /// Its lineage root.
    pub root_session_id: String,
    /// The call's ledger row, without `request_hash`.
    pub call: serde_json::Value,
    /// The session's totals.
    pub session_totals: UsageTotalsView,
    /// The lineage's totals.
    pub root_totals: UsageTotalsView,
    /// Every configured budget.
    pub budgets: Vec<BudgetStanding>,
}

method!(/// `system.metrics`: the metrics catalog, read locally.
    SystemMetricsCall = "system.metrics" (SystemParams) -> MetricsSnapshot);
method!(/// `system.health`: every component's health for the connection's
    /// profile.
    SystemHealthCall = "system.health" (SystemParams) -> SystemHealth);
event!(/// `usage.updated`: a model call of an attached session finished.
    UsageUpdatedEvent = "usage.updated" (UsageUpdated));
event!(/// `health.changed`: a component changed state.
    HealthChangedEvent = "health.changed" (HealthChanged));
