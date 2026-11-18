//! Opt-in OpenTelemetry export (spec: observability, Opt-in OpenTelemetry
//! export; Span model; Trace context propagation; Metrics catalog).
//!
//! Nothing is exported unless the launch profile sets
//! `observability.otel.enabled`; then [`start`] builds the process's one
//! [`Exporter`] from `[observability.otel]` (headers only from the secrets
//! it names; `OTEL_*` environment variables are never read) and installs
//! it, and every [`SessionTrace`] opened afterwards queues its spans
//! there.
//!
//! - [`context`]: `traceparent`, session trace ids, the live-session
//!   directory children and outbound calls read their parent from;
//! - [`spans`]: trajectory events -> spans (and the event-driven metrics);
//! - [`export`]: the bounded queue and the OTLP worker;
//! - [`proto`]: the OTLP protobuf messages.

pub mod context;
pub mod export;
pub mod metrics_proto;
pub mod proto;
pub mod spans;

use std::sync::{OnceLock, RwLock};

pub use context::TraceContext;
pub use export::{ExportSettings, ExportStats, Exporter};
pub use spans::{Identity, RedactorLoader, SessionTrace, Standalone};

use pmt_config::schema::OtelPreset;
use pmt_config::scope::ProfileScope;
use pmt_core::error::{Error, codes};

/// Every span name of the span model.
pub const SPAN_NAMES: &[&str] = &[
    "session",
    "turn",
    "model.call",
    "tool.call",
    "subagent",
    "compression",
    "approval.wait",
    "rpc.request",
    "cron.run",
    "eval.task",
    "evolve.stage",
];

fn slot() -> &'static RwLock<Option<Exporter>> {
    static SLOT: OnceLock<RwLock<Option<Exporter>>> = OnceLock::new();
    SLOT.get_or_init(RwLock::default)
}

/// The installed exporter, when export is on.
#[must_use]
pub fn exporter() -> Option<Exporter> {
    slot()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Installs `exporter` for every session opened from now on (`None`
/// uninstalls; tests).
pub fn install(exporter: Option<Exporter>) {
    *slot()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = exporter;
}

/// Langfuse's OTLP path under its host.
pub const LANGFUSE_PATH: &str = "/api/public/otel";

/// The Langfuse keys of `observability.otel.preset = "langfuse"`.
pub const LANGFUSE_KEYS: [&str; 2] = ["LANGFUSE_PUBLIC_KEY", "LANGFUSE_SECRET_KEY"];

/// Where export goes, and the preset that chose it: `None` when export is
/// off (a preset turns it on).
#[must_use]
pub fn target(
    observability: &pmt_config::schema::ObservabilityConfig,
) -> Option<(String, Option<&'static str>)> {
    let otel = &observability.otel;
    match otel.preset {
        Some(OtelPreset::Langfuse) => Some((
            format!(
                "{}{LANGFUSE_PATH}",
                observability.langfuse.host.trim_end_matches('/')
            ),
            Some("langfuse"),
        )),
        None if otel.enabled => otel
            .endpoint
            .clone()
            .filter(|e| !e.trim().is_empty())
            .map(|e| (e, None)),
        None => None,
    }
}

/// The secrets a preset needs, checked before a daemon starts serving.
///
/// # Errors
///
/// `config.invalid` (exit 78) naming the first missing secret.
pub fn preflight(scope: &ProfileScope) -> Result<(), Error> {
    preset_headers(scope).map(drop)
}

/// The headers a preset adds: Langfuse's Basic authentication.
fn preset_headers(scope: &ProfileScope) -> Result<Vec<(String, String)>, Error> {
    if scope.config().observability.otel.preset != Some(OtelPreset::Langfuse) {
        return Ok(Vec::new());
    }
    let mut keys = Vec::new();
    for name in LANGFUSE_KEYS {
        let value = scope.secrets().resolve(name).map_err(|_| {
            Error::new(
                &codes::CONFIG_INVALID,
                format!("observability.otel.preset = \"langfuse\" needs the secret {name}"),
            )
            .with_hint(format!("prometheus secrets set {name}"))
        })?;
        keys.push(value.expose().to_owned());
    }
    let basic = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, keys.join(":"));
    Ok(vec![("authorization".to_owned(), format!("Basic {basic}"))])
}

/// Starts and installs the exporter `scope`'s `[observability.otel]`
/// enables (or its preset); `Ok(None)` (and nothing configured to reach
/// the network) when export is off.
///
/// # Errors
///
/// `config.invalid` without an endpoint or without a preset's secret,
/// `secret.missing` for a missing header secret, HTTP client errors.
pub fn start(scope: &ProfileScope, service_instance: &str) -> Result<Option<Exporter>, Error> {
    let config = scope.config();
    let otel = &config.observability.otel;
    if !otel.enabled && otel.preset.is_none() {
        return Ok(None);
    }
    let (endpoint, preset) = target(&config.observability).ok_or_else(|| {
        Error::new(
            &codes::CONFIG_INVALID,
            "observability.otel.enabled = true requires observability.otel.endpoint",
        )
    })?;
    let mut headers = otel
        .header_secrets
        .iter()
        .map(|name| {
            let value = scope.secrets().resolve(name)?;
            Ok((header_name(name), value.expose().to_owned()))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    headers.extend(preset_headers(scope)?);
    let exporter = Exporter::start(ExportSettings {
        endpoint,
        protocol: if preset.is_some() {
            pmt_config::schema::OtelProtocol::HttpProtobuf
        } else {
            otel.protocol
        },
        headers,
        capture_content: otel.capture_content,
        propagate_external: otel.propagate_external,
        outbound: pmt_net::OutboundPolicy {
            force_ipv4: config.network.force_ipv4,
            extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
            proxy: config.network.proxy.clone(),
        },
        resource: vec![
            ("service.name".to_owned(), "prometheus".to_owned()),
            (
                "service.version".to_owned(),
                env!("CARGO_PKG_VERSION").to_owned(),
            ),
            (
                "service.instance.id".to_owned(),
                service_instance.to_owned(),
            ),
        ],
        metrics: preset.is_none(),
    })?;
    install(Some(exporter.clone()));
    Ok(Some(exporter))
}

/// The header a secret named `name` is sent as (`X_API_KEY` ->
/// `x-api-key`).
#[must_use]
pub fn header_name(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "-")
}

/// `OTEL_*` variables set in `vars`: they configure nothing (doctor warns).
#[must_use]
pub fn ignored_env_vars(vars: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = vars
        .into_iter()
        .filter(|k| k.starts_with("OTEL_"))
        .collect();
    names.sort();
    names
}
