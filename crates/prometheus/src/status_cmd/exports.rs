//! `prometheus status --exports` (spec: observability, Export posture
//! status): for each exporter whether it is enabled, its endpoint host
//! (never credentials), its last successful export, queued and dropped
//! items, and what can leave the host.

use pmt_config::ProfileScope;
use pmt_rpc::control::ExportReport;
use serde::Serialize;

/// One exporter's posture.
#[derive(Debug, Clone, Serialize)]
#[allow(clippy::struct_excessive_bools)] // independent report fields
pub(super) struct Exporter {
    /// `otlp_traces`, `otlp_metrics`, `otlp_logs`, `metrics_endpoint`,
    /// `gateway_health`, `nemo_relay`, `langfuse`.
    pub name: &'static str,
    /// It sends (or serves) anything.
    pub enabled: bool,
    /// Where it sends to (`127.0.0.1:<port>` for the scrape endpoint).
    pub endpoint_host: Option<String>,
    /// The last successful export (RFC 3339), when the daemon knows it.
    pub last_success: Option<String>,
    /// Items waiting.
    pub queued: Option<u64>,
    /// Items dropped.
    pub dropped: Option<u64>,
    /// Message content can leave the host through it.
    pub content: bool,
    /// Tool arguments can leave the host through it.
    pub tool_arguments: bool,
    /// User identifiers can leave the host through it.
    pub user_identifiers: bool,
}

/// The posture report.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Posture {
    /// Every exporter.
    pub exporters: Vec<Exporter>,
    /// `observability.otel.capture_content`.
    pub capture_content: bool,
    /// `observability.otel.propagate_external`.
    pub propagate_external: bool,
    /// Nothing is sent or served.
    pub nothing_leaves: bool,
}

fn host(url: Option<&str>) -> Option<String> {
    let url = reqwest::Url::parse(url?).ok()?;
    let host = url.host_str()?;
    Some(
        url.port()
            .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")),
    )
}

fn disabled(name: &'static str) -> Exporter {
    Exporter {
        name,
        enabled: false,
        endpoint_host: None,
        last_success: None,
        queued: None,
        dropped: None,
        content: false,
        tool_arguments: false,
        user_identifiers: false,
    }
}

/// The posture of `scope` (the daemon's launch profile), with the running
/// exporter's counters from `live`.
pub(super) fn posture(
    scope: &ProfileScope,
    live: Option<&ExportReport>,
    health_url: Option<&str>,
) -> Posture {
    let observability = &scope.config().observability;
    let otel = &observability.otel;
    let otel_host = host(otel.endpoint.as_deref());
    let otlp = |name: &'static str, sent: bool| Exporter {
        enabled: otel.enabled && sent,
        endpoint_host: (otel.enabled && sent).then(|| otel_host.clone()).flatten(),
        last_success: live.and_then(|l| l.last_success.clone()),
        queued: live.map(|l| l.queued),
        dropped: live.map(|l| l.dropped),
        content: otel.enabled && sent && otel.capture_content,
        tool_arguments: otel.enabled && sent && otel.capture_content,
        user_identifiers: false,
        ..disabled(name)
    };
    let scrape = observability.metrics_endpoint.enabled;
    let health = &observability.gateway_health;
    let exporters = vec![
        otlp("otlp_traces", true),
        otlp("otlp_metrics", true),
        // Log records are never exported; they stay in the local files.
        disabled("otlp_logs"),
        Exporter {
            enabled: scrape,
            endpoint_host: scrape
                .then(|| health_url.and_then(|u| host(Some(u))))
                .flatten(),
            ..disabled("metrics_endpoint")
        },
        Exporter {
            enabled: health.enabled,
            endpoint_host: health
                .enabled
                .then(|| host(health.endpoint.as_deref()))
                .flatten(),
            ..disabled("gateway_health")
        },
        disabled("nemo_relay"),
        disabled("langfuse"),
    ];
    let nothing_leaves = !exporters.iter().any(|e| e.enabled);
    Posture {
        exporters,
        capture_content: otel.capture_content,
        propagate_external: otel.propagate_external,
        nothing_leaves,
    }
}

/// The text report.
pub(super) fn render(posture: &Posture) -> String {
    use std::fmt::Write as _;
    if posture.nothing_leaves {
        return "no telemetry leaves this host\n".to_owned();
    }
    let mut text = String::new();
    for e in posture.exporters.iter().filter(|e| e.enabled) {
        let _ = write!(
            text,
            "{:<17}enabled -> {}",
            e.name,
            e.endpoint_host.as_deref().unwrap_or("?")
        );
        if let Some(at) = &e.last_success {
            let _ = write!(text, ", last export {at}");
        }
        if let (Some(queued), Some(dropped)) = (e.queued, e.dropped) {
            let _ = write!(text, ", {queued} queued, {dropped} dropped");
        }
        text.push('\n');
    }
    let disabled: Vec<&str> = posture
        .exporters
        .iter()
        .filter(|e| !e.enabled)
        .map(|e| e.name)
        .collect();
    let _ = writeln!(text, "disabled: {}", disabled.join(", "));
    let otlp_host = posture
        .exporters
        .iter()
        .find(|e| e.name == "otlp_traces" && e.enabled)
        .and_then(|e| e.endpoint_host.clone());
    match (posture.capture_content, otlp_host) {
        (true, Some(host)) => {
            let _ = writeln!(
                text,
                "message content and tool arguments are exported to {host} (capture_content = true)"
            );
        }
        _ => text.push_str(
            "message content, tool arguments and user identifiers do not leave this host\n",
        ),
    }
    let _ = writeln!(
        text,
        "traceparent to external hosts: {}",
        if posture.propagate_external {
            "sent (propagate_external = true)"
        } else {
            "not sent"
        }
    );
    text
}
