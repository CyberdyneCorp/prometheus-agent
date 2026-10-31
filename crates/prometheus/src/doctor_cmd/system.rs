//! The OS-facing groups: sandbox, terminal, network and security.

use std::path::PathBuf;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::doctor::{Check, Status};
use pmt_net::doctor::{Ipv6Check, PROBE_TIMEOUT, Target, has_global_ipv6_route};
use pmt_sandbox::backend::BackendRegistry;
use pmt_sandbox::{Enforcement, Paths, Policy, Sandbox};

use super::Inputs;

/// The baseline sandbox of this profile (what a session gets before
/// grants), or why there is none.
fn baseline(inputs: &Inputs<'_>) -> Result<Sandbox, String> {
    let scope = inputs.scope;
    let scratch = scope.path("cache/scratch");
    let paths = Paths {
        user_home: inputs
            .ctx
            .launch
            .user_home()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/")),
        prometheus_home: scope.home().to_path_buf(),
        workspace: std::env::current_dir().unwrap_or_else(|_| scratch.clone()),
        scratch: vec![scratch],
    };
    Policy::baseline(&paths)
        .and_then(|policy| Sandbox::new(policy, &scope.config().security.sandbox))
        .map_err(|err| err.message().to_owned())
}

/// `sandbox`: the mechanism and its strength on this OS.
pub(super) fn sandbox(inputs: &Inputs<'_>) -> Vec<Check> {
    let check = match baseline(inputs) {
        Ok(sandbox) => {
            let report = sandbox.report();
            let enforcement = report.enforcement();
            let status = match enforcement {
                Enforcement::Full => Status::Ok,
                Enforcement::Partial(_) | Enforcement::None => Status::Warn,
            };
            Check::new(
                "sandbox",
                "enforcement",
                status,
                format!("{}: {enforcement}", report.platform),
            )
            .with_hint("prometheus doctor security lists each feature")
        }
        Err(why) => Check::new("sandbox", "enforcement", Status::Fail, why)
            .with_hint("see docs/guide/sandbox.md"),
    };
    vec![check]
}

/// `terminal`: the selected backend and, for `local`, the shell.
pub(super) fn terminal(inputs: &Inputs<'_>) -> Vec<Check> {
    let config = &inputs.scope.config().terminal;
    let sandbox = match baseline(inputs) {
        Ok(sandbox) => sandbox,
        Err(why) => return vec![Check::new("terminal", "backend", Status::Fail, why)],
    };
    let shell = pmt_tools::terminal::doctor::local_shell(config, inputs.ctx.launch);
    pmt_tools::terminal::doctor::readiness(&BackendRegistry::builtin(), config, &sandbox, &shell)
        .checks
}

/// The configured providers' endpoints: `(provider, host, port)`.
pub(crate) fn provider_endpoints(scope: &ProfileScope) -> Vec<(String, String, u16)> {
    let config = scope.config();
    let Ok(registry) = pmt_providers::ProviderRegistry::for_profile(config, Vec::new()) else {
        return Vec::new();
    };
    let mut ids: Vec<&str> = config
        .model
        .provider
        .as_deref()
        .filter(|p| *p != "auto")
        .into_iter()
        .collect();
    ids.extend(config.fallback.chain.iter().map(|e| e.provider.as_str()));
    let mut endpoints = Vec::new();
    for id in ids {
        let Some(url) = registry
            .lookup(id)
            .ok()
            .and_then(|p| p.base_url())
            .and_then(|u| reqwest::Url::parse(u).ok())
        else {
            continue;
        };
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            continue;
        };
        let entry = (id.to_owned(), host.to_owned(), port);
        if !endpoints.contains(&entry) {
            endpoints.push(entry);
        }
    }
    endpoints
}

/// `network`: the IPv6 route (and, with `--online`, IPv6 reachability of
/// the provider hosts) and the egress mode.
pub(super) async fn network(inputs: &Inputs<'_>) -> Vec<Check> {
    let config = inputs.scope.config();
    let check = Ipv6Check {
        route: has_global_ipv6_route(),
        online: inputs.online,
        force_ipv4: config.network.force_ipv4,
        targets: provider_endpoints(inputs.scope)
            .into_iter()
            .map(|(_, host, port)| Target { host, port })
            .collect(),
        timeout: PROBE_TIMEOUT,
    };
    let mut checks = check.run(&pmt_net::resolve::SystemLookup).await;
    let egress = &config.security.egress;
    checks.push(Check::new(
        "network",
        "egress",
        Status::Ok,
        format!(
            "egress: direct (no proxy in this build); private destinations {}",
            if egress.allow_private {
                "allowed"
            } else {
                "blocked for tools"
            }
        ),
    ));
    checks
}

/// The trust boundaries (spec: security-boundaries, SECURITY.md).
const BOUNDARIES: &[&str] = &[
    "boundary: OS sandbox around local child processes",
    "boundary: container and remote terminal backends (none configured)",
    "boundary: WASM plugin instances (none installed)",
    "boundary: out-of-process sidecars (MCP stdio, Python bridge)",
    "boundary: authenticated external surfaces (none running)",
    "heuristics, not boundaries: approval gate, output redaction, skill scanning, provenance markers, taint tracking",
];

/// `security`: the boundaries, sandbox enforcement per feature, active
/// downgrades and the redaction state.
pub(super) fn security(inputs: &Inputs<'_>) -> Vec<Check> {
    let config = &inputs.scope.config().security;
    let mut checks: Vec<Check> = BOUNDARIES
        .iter()
        .map(|b| Check::new("security", "boundaries", Status::Ok, *b))
        .collect();
    match baseline(inputs) {
        Ok(sandbox) => {
            let sandbox = Arc::new(sandbox);
            checks.extend(sandbox.report().doctor_checks());
            for gap in sandbox.downgrades() {
                checks.push(
                    Check::new(
                        "security",
                        "downgrade",
                        Status::Warn,
                        format!("downgrade: {} ({})", gap.hardening.as_str(), gap.reason),
                    )
                    .with_hint(
                        "allowed by security.sandbox; remove the allowance to refuse instead",
                    ),
                );
            }
        }
        Err(why) => checks.push(Check::new("security", "sandbox", Status::Fail, why)),
    }
    if !config.protected_instruction_files {
        checks.push(
            Check::new(
                "security",
                "downgrade",
                Status::Warn,
                "downgrade: protected_instruction_files is off",
            )
            .with_hint("prometheus config set security.protected_instruction_files true"),
        );
    }
    checks.push(if config.redact_secrets {
        Check::new("security", "redaction", Status::Ok, "secret redaction on")
    } else {
        Check::new(
            "security",
            "redaction",
            Status::Warn,
            "secret redaction off",
        )
        .with_hint("prometheus config set security.redact_secrets true")
    });
    checks
}
