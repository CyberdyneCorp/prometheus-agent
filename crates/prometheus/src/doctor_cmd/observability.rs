//! `prometheus doctor observability` (spec: observability, Opt-in
//! OpenTelemetry export; `NeMo` Relay and Langfuse integrations): the
//! exporters in use, ignored `OTEL_*` variables, and which `NeMo` Relay
//! `plugins.toml` files apply (lowest precedence first; the system file
//! wins).

use pmt_obs::relay::{RelayFile, RelaySource};

use pmt_core::doctor::{Check, Status};

use super::Inputs;

/// The group's checks.
pub(super) fn observability(inputs: &Inputs<'_>) -> Vec<Check> {
    let mut checks = vec![exporters(inputs)];
    checks.extend(ignored_env(inputs));
    checks.extend(relay(inputs));
    checks
}

fn exporters(inputs: &Inputs<'_>) -> Check {
    let observability = &inputs.scope.config().observability;
    let trajectories = if observability.trajectories.enabled {
        "on"
    } else {
        "off"
    };
    let detail = match pmt_obs::otel::target(observability) {
        Some((endpoint, preset)) => {
            let host = reqwest::Url::parse(&endpoint)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_default();
            let via = preset.map(|p| format!(" ({p} preset)")).unwrap_or_default();
            format!("OTLP export to {host}{via}; trajectories {trajectories}")
        }
        None => format!("no exporter enabled; trajectories {trajectories}"),
    };
    Check::new("observability", "exporters", Status::Ok, detail)
}

/// `OTEL_*` variables configure nothing: say so instead of letting them
/// look effective.
fn ignored_env(inputs: &Inputs<'_>) -> Option<Check> {
    let ignored = pmt_obs::otel::ignored_env_vars(
        inputs
            .ctx
            .launch
            .inherited_vars()
            .map(|(name, _)| name.to_owned()),
    );
    (!ignored.is_empty()).then(|| {
        Check::new(
            "observability",
            "otel_env",
            Status::Warn,
            format!(
                "{} set but ignored: export is configured only by [observability.otel]",
                ignored.join(", ")
            ),
        )
        .with_hint("prometheus config set observability.otel.enabled true")
    })
}

/// One line per Relay file that applies, then whether the integration is
/// on.
fn relay(inputs: &Inputs<'_>) -> Vec<Check> {
    let relay = &inputs.scope.config().observability.nemo_relay;
    let files = crate::relay_host::files(inputs.scope, inputs.ctx.launch);
    let applied: Vec<&RelayFile> = files.iter().filter(|f| f.exists).collect();
    let state = if relay.enabled { "on" } else { "off" };
    let mut checks = vec![Check::new(
        "observability",
        "nemo_relay",
        Status::Ok,
        format!(
            "NeMo Relay {state}; {} plugins.toml file(s) apply",
            applied.len()
        ),
    )];
    for file in applied {
        let source = match file.source {
            RelaySource::User => "user",
            RelaySource::Configured => "observability.nemo_relay.plugins_toml",
            RelaySource::System => "system",
        };
        let precedence = if file.source == RelaySource::System {
            ", highest precedence"
        } else {
            ""
        };
        checks.push(Check::new(
            "observability",
            "nemo_relay_file",
            Status::Ok,
            format!("{} ({source}{precedence})", file.path.display()),
        ));
    }
    checks
}
