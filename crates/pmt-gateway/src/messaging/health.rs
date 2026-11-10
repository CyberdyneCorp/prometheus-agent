//! The gateway health export (spec: messaging-gateway, Gateway health
//! export is opt-in and content-free). Off unless the launch profile sets
//! `observability.gateway_health.enabled`; then every `interval_secs` the
//! allowlisted `prometheus.gateway.*`, `prometheus.adapter.*`,
//! `prometheus.outbox.*` and `prometheus.cron.*` gauges and counters are
//! posted as OTLP/HTTP JSON metrics. Attributes are a closed set of
//! bounded values (profile, platform, state, error code, admission,
//! lifecycle event): never message text, user ids, chat ids or secrets.
//! Recording is a map update on the message path; the export runs on its
//! own task, so an unreachable collector changes nothing but one warning a
//! minute.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use pmt_store::gateway::outbox::OutboundState;
use serde_json::{Value, json};
use tokio::time::Instant;
use tracing::Instrument as _;

use super::Shared;

/// Every metric the export may carry.
pub const METRICS: &[&str] = &[
    "prometheus.gateway.profiles",
    "prometheus.gateway.conversations",
    "prometheus.gateway.turns_running",
    "prometheus.gateway.admissions",
    "prometheus.gateway.lifecycle",
    "prometheus.gateway.paused",
    "prometheus.adapter.state",
    "prometheus.adapter.fatal",
    "prometheus.outbox.queued",
    "prometheus.outbox.failed",
    "prometheus.outbox.sent",
    "prometheus.outbox.send_failures",
    "prometheus.cron.runs",
];

/// Every attribute key a data point may carry (bounded values only).
pub const ATTRIBUTES: &[&str] = &["profile", "platform", "state", "code", "admission", "event"];

/// Least time between two logged export failures.
const WARN_EVERY: Duration = Duration::from_mins(1);
/// One export's deadline.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// How the export runs (from the launch profile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthOptions {
    /// The OTLP/HTTP metrics endpoint.
    pub endpoint: String,
    /// Time between exports.
    pub interval: Duration,
    /// `network.extra_ca_file`.
    pub extra_ca_file: Option<PathBuf>,
}

type Attributes = Vec<(&'static str, String)>;

/// Counters recorded on the message path.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    values: Mutex<BTreeMap<(&'static str, Attributes), u64>>,
}

impl Counters {
    /// Adds one to `metric` with `attributes`; a name or key outside the
    /// allowlist is dropped (and is a bug).
    pub(crate) fn add(&self, metric: &'static str, attributes: &[(&'static str, &str)]) {
        let allowed =
            METRICS.contains(&metric) && attributes.iter().all(|(key, _)| ATTRIBUTES.contains(key));
        debug_assert!(
            allowed,
            "{metric} {attributes:?} is outside the health allowlist"
        );
        if !allowed {
            return;
        }
        let attributes: Attributes = attributes
            .iter()
            .map(|(k, v)| (*k, (*v).to_owned()))
            .collect();
        *self
            .values
            .lock()
            .expect("counters poisoned")
            .entry((metric, attributes))
            .or_default() += 1;
    }

    fn snapshot(&self) -> Vec<((&'static str, Attributes), u64)> {
        self.values
            .lock()
            .expect("counters poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }
}

/// Logs an export failure at most once per [`WARN_EVERY`].
#[derive(Debug, Default)]
pub(crate) struct WarnThrottle {
    last: Mutex<Option<Instant>>,
}

impl WarnThrottle {
    pub(crate) fn due(&self, now: Instant) -> bool {
        let mut last = self.last.lock().expect("throttle poisoned");
        let due = last.is_none_or(|at| now.saturating_duration_since(at) >= WARN_EVERY);
        if due {
            *last = Some(now);
        }
        due
    }
}

/// Starts the export task.
pub(crate) fn spawn(shared: &Arc<Shared>, options: HealthOptions) {
    let span = tracing::info_span!("health_export");
    pmt_core::spawn::spawn(run(Arc::clone(shared), options).instrument(span));
}

async fn run(shared: Arc<Shared>, options: HealthOptions) {
    let policy = pmt_net::OutboundPolicy {
        extra_ca_file: options.extra_ca_file.clone(),
        ..pmt_net::OutboundPolicy::default()
    };
    let client = match pmt_net::client_builder(&policy).and_then(|b| {
        b.timeout(EXPORT_TIMEOUT).build().map_err(|e| {
            pmt_core::Error::new(&pmt_core::error::codes::NET_CLIENT_INIT, e.to_string())
        })
    }) {
        Ok(client) => client,
        Err(err) => {
            tracing::error!("gateway health export disabled: {err}");
            return;
        }
    };
    let started = Timestamp::now();
    let throttle = WarnThrottle::default();
    loop {
        tokio::select! {
            () = shared.stop.cancelled() => return,
            () = tokio::time::sleep(options.interval) => {}
        }
        let body = payload(&shared, started).to_string();
        let outcome = client
            .post(&options.endpoint)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await;
        let failure = match outcome {
            Ok(response) if response.status().is_success() => None,
            Ok(response) => Some(format!("HTTP {}", response.status().as_u16())),
            Err(err) => Some(err.to_string()),
        };
        if let Some(why) = failure
            && throttle.due(Instant::now())
        {
            tracing::warn!(
                endpoint = options.endpoint,
                "gateway health export failed: {why} (logged at most once a minute)"
            );
        }
    }
}

/// The gauges, read now.
fn gauges(shared: &Shared) -> Vec<(&'static str, Attributes, u64)> {
    let mut gauges = Vec::new();
    let profiles = shared.server.profiles();
    gauges.push((
        "prometheus.gateway.profiles",
        Vec::new(),
        profiles.len() as u64,
    ));
    let (conversations, running) = shared.lane_counts();
    gauges.push((
        "prometheus.gateway.conversations",
        Vec::new(),
        conversations,
    ));
    gauges.push(("prometheus.gateway.turns_running", Vec::new(), running));
    let paused = u64::from(pmt_core::pause::active(&shared.options.home).is_some());
    gauges.push(("prometheus.gateway.paused", Vec::new(), paused));
    for status in shared.adapters.statuses() {
        let mut attributes = vec![
            ("profile", status.profile.clone()),
            ("platform", status.platform.clone()),
            (
                "state",
                json!(status.state).as_str().unwrap_or_default().to_owned(),
            ),
        ];
        if let Some(code) = status.code {
            attributes.push(("code", code));
        }
        gauges.push(("prometheus.adapter.state", attributes, 1));
    }
    for profile in profiles {
        let Some(store) = shared.store(&profile) else {
            continue;
        };
        for (metric, state) in [
            ("prometheus.outbox.queued", OutboundState::Queued),
            ("prometheus.outbox.failed", OutboundState::Failed),
        ] {
            if let Ok(count) = store.outbox_count(state) {
                gauges.push((metric, vec![("profile", profile.clone())], count));
            }
        }
    }
    gauges
}

fn attributes_json(attributes: &Attributes) -> Value {
    Value::Array(
        attributes
            .iter()
            .map(|(k, v)| json!({"key": k, "value": {"stringValue": v}}))
            .collect(),
    )
}

/// The OTLP/HTTP JSON export request.
fn payload(shared: &Shared, started: Timestamp) -> Value {
    let now = Timestamp::now().as_nanosecond().to_string();
    let start = started.as_nanosecond().to_string();
    let mut metrics = Vec::new();
    for (name, attributes, value) in gauges(shared) {
        metrics.push(json!({
            "name": name,
            "gauge": {"dataPoints": [{
                "attributes": attributes_json(&attributes),
                "timeUnixNano": now,
                "asInt": value.to_string(),
            }]},
        }));
    }
    for ((name, attributes), value) in shared.health.snapshot() {
        metrics.push(json!({
            "name": name,
            "sum": {
                "aggregationTemporality": 2,
                "isMonotonic": true,
                "dataPoints": [{
                    "attributes": attributes_json(&attributes),
                    "startTimeUnixNano": start,
                    "timeUnixNano": now,
                    "asInt": value.to_string(),
                }],
            },
        }));
    }
    json!({"resourceMetrics": [{
        "resource": {"attributes": [
            {"key": "service.name", "value": {"stringValue": "prometheus-gateway"}}
        ]},
        "scopeMetrics": [{"scope": {"name": "prometheus.gateway"}, "metrics": metrics}],
    }]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_failures_are_logged_at_most_once_a_minute() {
        let throttle = WarnThrottle::default();
        let start = Instant::now();
        assert!(throttle.due(start));
        assert!(!throttle.due(start + Duration::from_secs(1)));
        assert!(!throttle.due(start + Duration::from_secs(59)));
        assert!(throttle.due(start + Duration::from_mins(1)));
    }

    #[test]
    fn every_metric_is_in_the_prometheus_namespaces() {
        for name in METRICS {
            assert!(
                [
                    "prometheus.gateway.",
                    "prometheus.adapter.",
                    "prometheus.outbox.",
                    "prometheus.cron."
                ]
                .iter()
                .any(|prefix| name.starts_with(prefix)),
                "{name}"
            );
        }
    }

    #[test]
    fn counters_keep_allowlisted_points_only() {
        let counters = Counters::default();
        counters.add(
            "prometheus.gateway.admissions",
            &[("platform", "telegram"), ("admission", "accepted")],
        );
        counters.add(
            "prometheus.gateway.admissions",
            &[("platform", "telegram"), ("admission", "accepted")],
        );
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].1, 2);
    }
}
