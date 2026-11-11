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
    let shell = pmt_tools::terminal::doctor::local_shell(config, inputs.ctx.launch, &sandbox);
    let registry = if BackendRegistry::builtin().select(&config.backend).is_ok() {
        BackendRegistry::builtin()
    } else {
        // A plugin backend: its own `check` answers.
        let cwd = std::env::current_dir().unwrap_or_else(|_| inputs.scope.dir().to_path_buf());
        let plugins = crate::plugins_host::for_session(inputs.scope, &cwd);
        crate::chat::session::backend::registry(&plugins, &pmt_plugins::host::CallInfo::default())
    };
    let mut checks = pmt_tools::terminal::doctor::readiness(
        &registry,
        config,
        &sandbox,
        &inputs.scope.config().security.sandbox,
        &shell,
    )
    .checks;
    if config.backend == "modal" || config.backend == pmt_sandbox::backend::MANAGED_SANDBOX {
        checks.push(modal_mode(inputs.scope));
    }
    // On Windows a shell started elsewhere finds `prometheus` only when
    // its directory is on PATH (spec: cli, Windows console behaviour).
    if cfg!(windows) {
        checks.push(install_on_path(inputs));
    }
    checks
}

/// Whether the directory of this binary is on `path` (`PATH`'s value).
fn on_path(dir: &std::path::Path, path: &std::ffi::OsStr) -> bool {
    let same = |entry: &std::path::Path| {
        let canonical = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        canonical(entry) == canonical(dir)
    };
    std::env::split_paths(path).any(|entry| same(&entry))
}

/// `terminal/install_on_path`: the install directory on `PATH`.
fn install_on_path(inputs: &Inputs<'_>) -> Check {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf));
    let path = inputs.ctx.launch.inherited("PATH").unwrap_or_default();
    match dir {
        Some(dir) if on_path(&dir, path) => Check::new(
            "terminal",
            "install_on_path",
            Status::Ok,
            format!("{} is on PATH", dir.display()),
        ),
        Some(dir) => Check::new(
            "terminal",
            "install_on_path",
            Status::Warn,
            format!("{} is not on PATH", dir.display()),
        )
        .with_hint("add it to PATH so new terminals find prometheus"),
        None => Check::new(
            "terminal",
            "install_on_path",
            Status::Warn,
            "the install directory is unknown".to_owned(),
        ),
    }
}

/// The first-party `modal` backend's authentication mode, or why neither
/// mode is available.
fn modal_mode(scope: &ProfileScope) -> Check {
    match crate::chat::session::modal::of_profile(scope) {
        Ok(mode) => Check::new(
            "terminal",
            "modal_mode",
            Status::Ok,
            format!("mode: {mode}"),
        ),
        Err(why) => Check::new(
            "terminal",
            "modal_mode",
            Status::Fail,
            format!("modal backend unavailable: {why}"),
        ),
    }
}

/// The configured providers' base URLs: `(provider, url)`.
pub(crate) fn provider_urls(scope: &ProfileScope) -> Vec<(String, String)> {
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
    let mut urls: Vec<(String, String)> = Vec::new();
    for id in ids {
        if let Some(url) = registry.lookup(id).ok().and_then(|p| p.base_url()) {
            let entry = (id.to_owned(), url.to_owned());
            if !urls.contains(&entry) {
                urls.push(entry);
            }
        }
    }
    urls
}

/// The configured providers' endpoints: `(provider, host, port)`.
pub(crate) fn provider_endpoints(scope: &ProfileScope) -> Vec<(String, String, u16)> {
    let mut endpoints = Vec::new();
    for (id, url) in provider_urls(scope) {
        let Ok(url) = reqwest::Url::parse(&url) else {
            continue;
        };
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            continue;
        };
        let entry = (id, host.to_owned(), port);
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
    checks.push(Check::new(
        "network",
        "egress",
        Status::Ok,
        egress_line(&config.security.egress),
    ));
    if inputs.online {
        let network = &config.network;
        let open = super::isolation::direct_egress(&super::isolation::EGRESS_PROBES);
        checks.push(super::isolation::egress_check(
            open,
            network.expect_isolated,
        ));
        if network.expect_isolated || network.proxy.is_some() {
            checks.extend(super::isolation::proxy_checks(
                network.proxy.as_deref(),
                &provider_endpoints(inputs.scope),
            ));
        }
    }
    checks
}

/// The egress mode, its allow list and the private-range setting.
fn egress_line(egress: &pmt_config::schema::EgressConfig) -> String {
    let allow = if egress.allow.is_empty() {
        String::new()
    } else {
        format!(" ({})", egress.allow.join(", "))
    };
    format!(
        "egress: {}{allow}; sandboxed children through a per-session loopback proxy; private destinations {}",
        egress.mode.as_str(),
        if egress.allow_private {
            "allowed"
        } else {
            "blocked"
        }
    )
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
    checks.push(Check::new(
        "security",
        "egress",
        Status::Ok,
        egress_line(&config.egress),
    ));
    checks.extend(posture(inputs));
    checks.extend(advisories(inputs));
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

/// Supply-chain advisories matching this install, acknowledged ones
/// included (an acknowledgement silences the startup warning only).
fn advisories(inputs: &Inputs<'_>) -> Vec<Check> {
    let acked = &inputs.scope.config().security.acked_advisories;
    let found = crate::advisories::matches(inputs.scope);
    if found.is_empty() {
        return vec![Check::new(
            "security",
            "advisories",
            Status::Ok,
            "no installed package matches a supply-chain advisory",
        )];
    }
    found
        .iter()
        .map(|m| {
            let advisory = &m.advisory;
            let note = if acked.iter().any(|a| a.eq_ignore_ascii_case(&advisory.id)) {
                " (acknowledged)"
            } else {
                ""
            };
            Check::new(
                "security",
                "advisory",
                Status::Warn,
                format!(
                    "advisory {}{note}: {} {} ({}) is compromised: {}",
                    advisory.id,
                    m.installed.name,
                    m.installed.version,
                    m.installed.location,
                    advisory.summary
                ),
            )
            .with_hint(advisory.remediation.clone())
        })
        .collect()
}

/// The posture findings beyond the sandbox: unsigned plugins, surfaces
/// that admit everyone or have no allowlist, and file-mode violations
/// (spec: security-boundaries, Security posture check).
fn posture(inputs: &Inputs<'_>) -> Vec<Check> {
    let mut checks = Vec::new();
    for id in super::groups::unsigned_plugins(inputs) {
        checks.push(
            Check::new(
                "security",
                "unsigned_plugin",
                Status::Warn,
                format!("unsigned plugin: {id}"),
            )
            .with_hint("reinstall it from a signed catalog package, or remove it"),
        );
    }
    let platforms = &inputs.scope.config().gateway.platforms;
    for (name, platform) in platforms.iter().filter(|(_, p)| p.enabled) {
        let message = if platform.allow_all {
            format!("open surface: {name} admits every sender (allow_all)")
        } else if platform.allow_users.is_empty()
            && platform.allow_chats.is_empty()
            && platform.allow_roles.is_empty()
        {
            format!(
                "surface without allowlist: {name} refuses every sender until one is configured"
            )
        } else {
            continue;
        };
        checks.push(
            Check::new("security", "surface", Status::Warn, message).with_hint(format!(
                "set gateway.platforms.{name}.allow_users (or allow_chats, allow_roles)"
            )),
        );
    }
    for check in &inputs.file_modes {
        if check.status != Status::Ok {
            checks.push(
                Check::new(
                    "security",
                    "file_modes",
                    check.status,
                    format!("file modes: {}", check.message),
                )
                .with_hint("prometheus doctor --fix"),
            );
        }
    }
    if checks.is_empty() {
        checks.push(Check::new(
            "security",
            "posture",
            Status::Ok,
            "no unsigned plugins, open surfaces or loose file modes",
        ));
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::on_path;

    #[test]
    fn the_install_directory_is_found_among_the_path_entries() {
        let dir = std::env::temp_dir();
        let path = std::env::join_paths([std::path::Path::new("/nowhere"), &dir]).unwrap();
        assert!(on_path(&dir, &path));
        let other = std::env::join_paths([std::path::Path::new("/nowhere")]).unwrap();
        assert!(!on_path(&dir, &other));
    }
}
