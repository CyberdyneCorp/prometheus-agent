//! The metrics catalog (spec: observability, Metrics catalog): one
//! process-wide registry of the catalog's counters, histograms and gauges,
//! read locally through `system.metrics`, the daemon's `/metrics` scrape
//! endpoint ([`text`]) and, when enabled, exported over OTLP.
//!
//! Only catalog names and allowlisted attribute keys are recorded; a name
//! or key outside them is dropped (and is a bug, caught in debug builds),
//! so message content, file paths and user identifiers cannot become
//! labels. Recording is a map update under a short lock.

pub mod text;

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock, PoisonError};

use serde::Serialize;

/// What a metric is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Monotonic sum.
    Counter,
    /// Distribution in [`BUCKETS_MS`].
    Histogram,
    /// Current value.
    Gauge,
}

/// One catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    /// The metric name (`prometheus.tokens`).
    pub name: &'static str,
    /// Counter, histogram or gauge.
    pub kind: Kind,
    /// The UCUM unit (`{token}`, `USD`, `ms`, `1`).
    pub unit: &'static str,
    /// What it counts.
    pub description: &'static str,
}

const fn spec(
    name: &'static str,
    kind: Kind,
    unit: &'static str,
    description: &'static str,
) -> Spec {
    Spec {
        name,
        kind,
        unit,
        description,
    }
}

/// `prometheus.tokens`.
pub const TOKENS: &str = "prometheus.tokens";
/// `prometheus.cost.usd`.
pub const COST_USD: &str = "prometheus.cost.usd";
/// `prometheus.model.calls`.
pub const MODEL_CALLS: &str = "prometheus.model.calls";
/// `prometheus.tool.calls`.
pub const TOOL_CALLS: &str = "prometheus.tool.calls";
/// `prometheus.turns`.
pub const TURNS: &str = "prometheus.turns";
/// `prometheus.errors`.
pub const ERRORS: &str = "prometheus.errors";
/// `prometheus.budget.exceeded`.
pub const BUDGET_EXCEEDED: &str = "prometheus.budget.exceeded";
/// `prometheus.rpc.contract_violations`.
pub const RPC_CONTRACT_VIOLATIONS: &str = "prometheus.rpc.contract_violations";
/// `prometheus.otel.dropped`.
pub const OTEL_DROPPED: &str = "prometheus.otel.dropped";
/// `prometheus.model.latency_ms`.
pub const MODEL_LATENCY_MS: &str = "prometheus.model.latency_ms";
/// `prometheus.model.ttft_ms`.
pub const MODEL_TTFT_MS: &str = "prometheus.model.ttft_ms";
/// `prometheus.tool.duration_ms`.
pub const TOOL_DURATION_MS: &str = "prometheus.tool.duration_ms";
/// `prometheus.turn.duration_ms`.
pub const TURN_DURATION_MS: &str = "prometheus.turn.duration_ms";
/// `prometheus.sessions.active`.
pub const SESSIONS_ACTIVE: &str = "prometheus.sessions.active";
/// `prometheus.subagents.active`.
pub const SUBAGENTS_ACTIVE: &str = "prometheus.subagents.active";

/// Every metric of the catalog, in spec order.
pub const CATALOG: &[Spec] = &[
    spec(
        TOKENS,
        Kind::Counter,
        "{token}",
        "Model tokens by direction",
    ),
    spec(
        COST_USD,
        Kind::Counter,
        "USD",
        "Priced model and metered tool cost",
    ),
    spec(
        MODEL_CALLS,
        Kind::Counter,
        "{call}",
        "Model calls by outcome",
    ),
    spec(
        TOOL_CALLS,
        Kind::Counter,
        "{call}",
        "Tool calls by tool and outcome",
    ),
    spec(TURNS, Kind::Counter, "{turn}", "Finished turns"),
    spec(ERRORS, Kind::Counter, "{error}", "Errors by kind"),
    spec(
        BUDGET_EXCEEDED,
        Kind::Counter,
        "{call}",
        "Calls refused by a budget, by scope",
    ),
    spec(
        RPC_CONTRACT_VIOLATIONS,
        Kind::Counter,
        "{message}",
        "Outbound RPC messages that broke their contract",
    ),
    spec(
        OTEL_DROPPED,
        Kind::Counter,
        "{item}",
        "OTLP items dropped (queue overflow or failed export)",
    ),
    spec(
        MODEL_LATENCY_MS,
        Kind::Histogram,
        "ms",
        "Model call latency",
    ),
    spec(
        MODEL_TTFT_MS,
        Kind::Histogram,
        "ms",
        "Time to the first streamed token",
    ),
    spec(
        TOOL_DURATION_MS,
        Kind::Histogram,
        "ms",
        "Tool call duration",
    ),
    spec(TURN_DURATION_MS, Kind::Histogram, "ms", "Turn duration"),
    spec(SESSIONS_ACTIVE, Kind::Gauge, "{session}", "Open sessions"),
    spec(
        SUBAGENTS_ACTIVE,
        Kind::Gauge,
        "{subagent}",
        "Running subagents",
    ),
];

/// Every attribute key a data point may carry (bounded values only).
pub const ATTRIBUTES: &[&str] = &[
    "direction",
    "provider",
    "model",
    "purpose",
    "outcome",
    "tool",
    "kind",
    "scope",
];

/// Upper bucket bounds of every histogram, in milliseconds.
pub const BUCKETS_MS: &[f64] = &[
    5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, 30_000.0,
    60_000.0, 120_000.0, 300_000.0,
];

/// A data point's attributes, sorted by key.
pub type Attributes = Vec<(String, String)>;

/// One histogram's buckets.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Distribution {
    /// Observations.
    pub count: u64,
    /// Their sum.
    pub sum: f64,
    /// Observations per bucket of [`BUCKETS_MS`], plus one overflow bucket.
    pub buckets: Vec<u64>,
}

impl Distribution {
    fn observe(&mut self, value: f64) {
        if self.buckets.is_empty() {
            self.buckets = vec![0; BUCKETS_MS.len() + 1];
        }
        let index = BUCKETS_MS
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(BUCKETS_MS.len());
        self.buckets[index] += 1;
        self.count += 1;
        self.sum += value;
    }
}

/// A data point's value.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Value {
    /// Counter or gauge.
    Number(f64),
    /// Histogram.
    Distribution(Distribution),
}

/// One data point.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Point {
    /// Its attributes.
    pub attributes: BTreeMap<String, String>,
    /// Its value.
    pub value: Value,
}

/// One metric with every data point recorded so far.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metric {
    /// The name.
    pub name: &'static str,
    /// Counter, histogram or gauge.
    pub kind: Kind,
    /// Its unit.
    pub unit: &'static str,
    /// Its description.
    pub description: &'static str,
    /// Its data points.
    pub points: Vec<Point>,
}

/// Every metric of the catalog at one moment.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    /// When the registry started counting (Unix nanoseconds).
    pub start_unix_nano: u64,
    /// When the snapshot was taken (Unix nanoseconds).
    pub time_unix_nano: u64,
    /// Every catalog metric, in catalog order (with no points when never
    /// recorded).
    pub metrics: Vec<Metric>,
}

impl Snapshot {
    /// The value of a counter or gauge data point whose attributes include
    /// every pair of `attributes` (summed over the matching points).
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // observation counts far below 2^52
    pub fn value(&self, name: &str, attributes: &[(&str, &str)]) -> f64 {
        self.metrics
            .iter()
            .filter(|m| m.name == name)
            .flat_map(|m| &m.points)
            .filter(|p| {
                attributes
                    .iter()
                    .all(|(k, v)| p.attributes.get(*k).is_some_and(|x| x == v))
            })
            .map(|p| match &p.value {
                Value::Number(n) => *n,
                Value::Distribution(d) => d.count as f64,
            })
            .sum()
    }
}

#[derive(Debug, Default)]
struct Series {
    numbers: BTreeMap<(&'static str, Attributes), f64>,
    distributions: BTreeMap<(&'static str, Attributes), Distribution>,
}

/// The process's metrics.
#[derive(Debug)]
pub struct Registry {
    started: u64,
    series: Mutex<Series>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            started: unix_nanos(),
            series: Mutex::default(),
        }
    }
}

/// The process-wide registry.
#[must_use]
pub fn global() -> &'static Registry {
    static GLOBAL: OnceLock<Registry> = OnceLock::new();
    GLOBAL.get_or_init(Registry::default)
}

impl Registry {
    /// Adds `value` to a counter.
    pub fn add(&self, name: &'static str, attributes: &[(&str, &str)], value: f64) {
        if let Some(key) = key(name, Kind::Counter, attributes) {
            *self.lock().numbers.entry(key).or_default() += value;
        }
    }

    /// Records one observation of a histogram.
    pub fn observe(&self, name: &'static str, attributes: &[(&str, &str)], value: f64) {
        if let Some(key) = key(name, Kind::Histogram, attributes) {
            self.lock()
                .distributions
                .entry(key)
                .or_default()
                .observe(value);
        }
    }

    /// Moves a gauge by `delta` (never below zero).
    pub fn gauge_add(&self, name: &'static str, attributes: &[(&str, &str)], delta: f64) {
        if let Some(key) = key(name, Kind::Gauge, attributes) {
            let mut series = self.lock();
            let value = series.numbers.entry(key).or_default();
            *value = (*value + delta).max(0.0);
        }
    }

    /// Every catalog metric now.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let series = self.lock();
        let metrics = CATALOG
            .iter()
            .map(|spec| {
                let numbers = series
                    .numbers
                    .iter()
                    .filter(|((name, _), _)| *name == spec.name)
                    .map(|((_, attrs), v)| point(attrs, Value::Number(*v)));
                let distributions = series
                    .distributions
                    .iter()
                    .filter(|((name, _), _)| *name == spec.name)
                    .map(|((_, attrs), d)| point(attrs, Value::Distribution(d.clone())));
                Metric {
                    name: spec.name,
                    kind: spec.kind,
                    unit: spec.unit,
                    description: spec.description,
                    points: numbers.chain(distributions).collect(),
                }
            })
            .collect();
        Snapshot {
            start_unix_nano: self.started,
            time_unix_nano: unix_nanos(),
            metrics,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Series> {
        self.series.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn point(attributes: &Attributes, value: Value) -> Point {
    Point {
        attributes: attributes.iter().cloned().collect(),
        value,
    }
}

/// The series key, or `None` (dropped) for a name outside the catalog, a
/// kind mismatch or an attribute key outside the allowlist.
fn key(
    name: &'static str,
    kind: Kind,
    attributes: &[(&str, &str)],
) -> Option<(&'static str, Attributes)> {
    let known = CATALOG.iter().any(|s| s.name == name && s.kind == kind);
    let allowed = attributes.iter().all(|(k, _)| ATTRIBUTES.contains(k));
    debug_assert!(
        known && allowed,
        "{name} {attributes:?} is outside the metrics catalog"
    );
    if !known || !allowed {
        return None;
    }
    let mut attrs: Attributes = attributes
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    attrs.sort();
    Some((name, attrs))
}

/// Now, in Unix nanoseconds.
#[must_use]
pub fn unix_nanos() -> u64 {
    u64::try_from(jiff::Timestamp::now().as_nanosecond()).unwrap_or(0)
}

/// Records a finished model call's ledger row: tokens per direction, cost
/// and one successful call (spec: observability, Metrics catalog).
pub fn record_model_call(row: &pmt_store::UsageEvent) {
    let registry = global();
    let base = [
        ("provider", row.provider.as_str()),
        ("model", row.model.as_str()),
        ("purpose", row.purpose.as_str()),
    ];
    if let Some(cost) = row.cost_usd {
        registry.add(COST_USD, &base, cost);
    }
    if row.purpose.starts_with("tool.") {
        return; // a metered tool call: cost only, never a model call
    }
    let t = &row.tokens;
    for (direction, count) in [
        ("input", t.input),
        ("output", t.output),
        ("cache_read", t.cache_read),
        ("cache_write", t.cache_write),
        ("reasoning", t.reasoning),
    ] {
        if count > 0 {
            let mut attrs = base.to_vec();
            attrs.push(("direction", direction));
            #[allow(clippy::cast_precision_loss)] // token counts far below 2^52
            registry.add(TOKENS, &attrs, count as f64);
        }
    }
    let mut attrs = base.to_vec();
    attrs.push(("outcome", "ok"));
    registry.add(MODEL_CALLS, &attrs, 1.0);
}

/// Records a model call that failed with error kind `kind` (`auth`,
/// `rate_limit`...).
pub fn record_model_failure(provider: &str, model: &str, kind: &str) {
    let registry = global();
    registry.add(
        MODEL_CALLS,
        &[
            ("provider", provider),
            ("model", model),
            ("outcome", "error"),
        ],
        1.0,
    );
    registry.add(ERRORS, &[("kind", kind)], 1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_sum_per_attribute_set_and_unknown_keys_never_become_labels() {
        let registry = Registry::default();
        registry.add(ERRORS, &[("kind", "auth")], 1.0);
        registry.add(ERRORS, &[("kind", "auth")], 1.0);
        registry.add(ERRORS, &[("kind", "network")], 1.0);
        registry.observe(MODEL_LATENCY_MS, &[], 12.0);
        registry.gauge_add(SESSIONS_ACTIVE, &[], -1.0);
        let snapshot = registry.snapshot();
        assert!((snapshot.value(ERRORS, &[("kind", "auth")]) - 2.0).abs() < f64::EPSILON);
        assert!((snapshot.value(ERRORS, &[]) - 3.0).abs() < f64::EPSILON);
        assert!((snapshot.value(MODEL_LATENCY_MS, &[]) - 1.0).abs() < f64::EPSILON);
        assert!(snapshot.value(SESSIONS_ACTIVE, &[]).abs() < f64::EPSILON);
        assert_eq!(snapshot.metrics.len(), CATALOG.len());
    }

    #[test]
    fn histogram_observations_land_in_their_bucket() {
        let mut d = Distribution::default();
        d.observe(7.0);
        d.observe(1e9);
        assert_eq!(d.buckets[1], 1);
        assert_eq!(d.buckets[BUCKETS_MS.len()], 1);
        assert_eq!(d.count, 2);
    }
}
