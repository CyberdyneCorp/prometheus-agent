//! The python bridge in the `plugins` group, when a python plugin is
//! installed: how strongly a sidecar's network is confined (spec:
//! python-bridge, Capability grants enforced at the process boundary) and
//! which facility bounds its resources (Resource limits).

use pmt_core::doctor::{Check, Status};
use pmt_plugins::manifest::Kind;
use pmt_sandbox::{Feature, Paths, Policy, Sandbox};

use super::Inputs;

/// How each OS bounds a sidecar's memory and processes.
#[cfg(target_os = "linux")]
const LIMITS: (Status, &str) = (
    Status::Ok,
    "OS data-segment rlimit per process, sidecar is subreaper of its tree; tree totals sampled every 500 ms",
);
#[cfg(windows)]
const LIMITS: (Status, &str) = (
    Status::Ok,
    "Job Object committed-memory and active-process limits for the whole tree",
);
#[cfg(not(any(target_os = "linux", windows)))]
const LIMITS: (Status, &str) = (
    Status::Warn,
    "tree sampled every 500 ms (this OS accepts no per-tree memory or process rlimit), so usage may overshoot for one sample",
);

/// The bridge checks, or none without a python plugin.
pub(super) fn checks(inputs: &Inputs<'_>) -> Vec<Check> {
    let scope = inputs.scope;
    let version = crate::plugins_host::running_version();
    let blocked = crate::plugins_host::catalog::kill_list(scope);
    let cwd = std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf());
    let surveyed = pmt_plugins::load::survey(&crate::plugins_host::inputs(
        scope, &cwd, &version, &blocked,
    ));
    let python = surveyed.iter().any(|(plugin, _)| {
        plugin
            .manifest
            .as_ref()
            .is_ok_and(|m| m.kind == Kind::Python)
    });
    if !python {
        return Vec::new();
    }
    let (status, text) = LIMITS;
    vec![
        network(inputs),
        Check::new(
            "plugins",
            "bridge_limits",
            status,
            format!("python bridge resource limits: {text}"),
        ),
    ]
}

/// The sidecar's network: only its egress proxy, as directly as the OS
/// sandbox can deny other sockets.
fn network(inputs: &Inputs<'_>) -> Check {
    let scope = inputs.scope;
    let paths = Paths {
        user_home: std::env::var_os("HOME").map(Into::into).unwrap_or_default(),
        prometheus_home: scope.home().to_path_buf(),
        workspace: scope.dir().join("plugin-data"),
        scratch: Vec::new(),
    };
    let sandbox = Policy::from_capabilities(&["net.connect:*:1"], &paths)
        .and_then(|policy| Sandbox::new(policy, &scope.config().security.sandbox));
    let sandbox = match sandbox {
        Ok(sandbox) => sandbox,
        Err(err) => {
            return Check::new(
                "plugins",
                "bridge_net",
                Status::Fail,
                format!("python bridge network: {}", err.message()),
            );
        }
    };
    let Some(net) = sandbox
        .report()
        .features
        .iter()
        .find(|f| f.feature == Feature::Net)
    else {
        return Check::new(
            "plugins",
            "bridge_net",
            Status::Warn,
            "python bridge network: egress proxy only; this OS sandbox reports no network enforcement",
        );
    };
    let message = format!(
        "python bridge network: egress proxy for granted hosts; direct sockets {} by {}",
        net.level.as_str(),
        net.mechanism
    );
    match &net.gap {
        None => Check::new("plugins", "bridge_net", Status::Ok, message),
        Some(gap) => Check::new(
            "plugins",
            "bridge_net",
            Status::Warn,
            format!("{message} ({})", gap.reason),
        ),
    }
}
