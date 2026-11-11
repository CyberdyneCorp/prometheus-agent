//! `system.metrics` and `system.health` (spec: observability, Metrics
//! catalog; Component health model): the process's metrics catalog and
//! the component health of the connection's profile, read from the
//! process-wide registries.

use pmt_obs::metrics::{self, Value};
use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::profile_for;
use crate::router::Router;
use crate::server::Call;

/// Registers the observability methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::SystemMetricsCall, _, _>(catalog, system_metrics)?
        .register::<c::SystemHealthCall, _, _>(catalog, system_health)?;
    Ok(())
}

async fn system_metrics(
    call: Call,
    params: c::SystemParams,
) -> Result<c::MetricsSnapshot, RpcError> {
    profile_for(&call, params.profile.as_deref())?;
    Ok(snapshot(&metrics::global().snapshot()))
}

async fn system_health(call: Call, params: c::SystemParams) -> Result<c::SystemHealth, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    Ok(health(Some(&profile)))
}

/// The metrics catalog as its contract.
#[must_use]
pub fn snapshot(snapshot: &metrics::Snapshot) -> c::MetricsSnapshot {
    c::MetricsSnapshot {
        bounds: metrics::BUCKETS_MS.to_vec(),
        metrics: snapshot
            .metrics
            .iter()
            .map(|m| c::MetricView {
                name: m.name.to_owned(),
                kind: serde_json::to_value(m.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                unit: m.unit.to_owned(),
                description: m.description.to_owned(),
                points: m
                    .points
                    .iter()
                    .map(|p| match &p.value {
                        Value::Number(v) => c::MetricPoint {
                            attributes: p.attributes.clone(),
                            value: *v,
                            sum: None,
                            buckets: None,
                        },
                        #[allow(clippy::cast_precision_loss)] // counts far below 2^52
                        Value::Distribution(d) => c::MetricPoint {
                            attributes: p.attributes.clone(),
                            value: d.count as f64,
                            sum: Some(d.sum),
                            buckets: Some(d.buckets.clone()),
                        },
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// The components of `profile` and the daemon-wide ones (`None`: all).
#[must_use]
pub fn health(profile: Option<&str>) -> c::SystemHealth {
    c::SystemHealth {
        components: pmt_obs::health::global()
            .components(profile)
            .into_iter()
            .map(|component| c::ComponentHealth {
                component: component.name,
                profile: component.profile,
                state: component.state.as_str().to_owned(),
                reason: component.reason,
                since: component.since,
            })
            .collect(),
    }
}

/// Every component, for `daemon status` and `prometheus status`.
#[must_use]
pub fn component_reports() -> Vec<pmt_rpc::control::ComponentReport> {
    pmt_obs::health::global()
        .components(None)
        .into_iter()
        .map(|c| pmt_rpc::control::ComponentReport {
            component: c.name,
            profile: c.profile,
            state: c.state.as_str().to_owned(),
            reason: c.reason,
            since: c.since,
        })
        .collect()
}

/// Subagents running now (`prometheus.subagents.active`).
#[must_use]
pub fn subagents() -> usize {
    let running = metrics::global()
        .snapshot()
        .value(metrics::SUBAGENTS_ACTIVE, &[]);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a small count
    let running = running.max(0.0) as usize;
    running
}

/// The OTLP exporter's posture and counters, when export is on.
#[must_use]
pub fn export_report() -> Option<pmt_rpc::control::ExportReport> {
    let exporter = pmt_obs::otel::exporter()?;
    let stats = exporter.stats();
    Some(pmt_rpc::control::ExportReport {
        host: exporter.host().to_owned(),
        protocol: match exporter.protocol() {
            pmt_config::schema::OtelProtocol::HttpProtobuf => "http/protobuf",
            pmt_config::schema::OtelProtocol::Grpc => "grpc",
        }
        .to_owned(),
        capture_content: exporter.captures_content(),
        propagate_external: exporter.propagates_external(),
        queued: stats.queued,
        dropped: stats.dropped,
        exported: stats.exported,
        last_success: stats.last_success,
        last_error: stats.last_error,
    })
}

/// Sends every component transition as `health.changed` to the attached
/// clients of its profile (a daemon-wide component: of every served
/// profile), while `server` lives.
pub fn announce_health(server: &std::sync::Arc<crate::Server>) {
    let weak = std::sync::Arc::downgrade(server);
    pmt_obs::health::global().subscribe(move |change| {
        let Some(server) = weak.upgrade() else {
            return;
        };
        let c = &change.component;
        let payload = serde_json::json!({
            "component": c.name,
            "profile": c.profile,
            "state": c.state.as_str(),
            "previous": change.previous.map(pmt_obs::health::State::as_str),
            "reason": c.reason,
            "since": c.since,
        });
        let profiles = c
            .profile
            .clone()
            .map_or_else(|| server.profiles(), |p| vec![p]);
        for profile in profiles {
            server.hub().broadcast(
                &profile,
                "health.changed",
                payload.clone(),
                crate::hub::NO_CONN,
            );
        }
    });
}
