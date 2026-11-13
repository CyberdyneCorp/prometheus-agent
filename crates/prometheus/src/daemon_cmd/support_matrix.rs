//! The platform support matrix (spec: web-dashboard, Honest platform
//! support matrix), probed once when the daemon starts and served in
//! `rpc.hello` and `/api/system/platform`.
//!
//! Each row says what the probe found and who lacks what: a reason names
//! the operating system only when the OS lacks the mechanism (no Landlock
//! in this kernel) and the dependency when a program or service is missing
//! (no Docker on the `PATH`), never one for the other.

use std::path::{Path, PathBuf};

use pmt_config::schema::SandboxConfig;
use pmt_rpc::contracts::FeatureSupport;
use pmt_sandbox::{Enforcement, Paths, Policy, Sandbox};

fn row(feature: &str, state: &str, reason: impl Into<String>) -> FeatureSupport {
    FeatureSupport {
        feature: feature.to_owned(),
        state: state.to_owned(),
        reason: reason.into(),
    }
}

/// The local OS sandbox: the enforcement this host gives the baseline
/// policy, each gap with the reason the sandbox reported.
fn sandbox(home: &Path) -> FeatureSupport {
    let scratch = std::env::temp_dir();
    let paths = Paths {
        user_home: home.to_path_buf(),
        prometheus_home: home.to_path_buf(),
        workspace: scratch.clone(),
        scratch: vec![scratch],
    };
    let probed =
        Policy::baseline(&paths).and_then(|policy| Sandbox::new(policy, &SandboxConfig::default()));
    let sandbox = match probed {
        Ok(sandbox) => sandbox,
        Err(err) => {
            return row(
                "sandbox.local",
                "unsupported",
                format!("the sandbox could not be prepared: {}", err.message()),
            );
        }
    };
    let report = sandbox.report();
    match report.enforcement() {
        Enforcement::Full => row(
            "sandbox.local",
            "supported",
            format!("{} enforces every feature", report.platform),
        ),
        Enforcement::None => {
            let why: Vec<String> = report.gaps().iter().map(|g| g.reason.clone()).collect();
            row("sandbox.local", "unsupported", why.join("; "))
        }
        Enforcement::Partial(_) => {
            let why: Vec<String> = report
                .gaps()
                .iter()
                .map(|g| format!("{}: {}", g.hardening.as_str(), g.reason))
                .collect();
            row(
                "sandbox.local",
                "degraded",
                format!(
                    "{} enforces partially ({})",
                    report.platform,
                    why.join("; ")
                ),
            )
        }
    }
}

/// A program on the daemon's `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidates = [dir.join(name), dir.join(format!("{name}.exe"))];
        candidates.into_iter().find(|p| p.is_file())
    })
}

/// Container sessions need Podman or Docker (a dependency, any OS).
fn containers() -> FeatureSupport {
    match pmt_sandbox::container::Runtime::detect("auto") {
        Ok(runtime) => row(
            "containers",
            "supported",
            format!("{} found", runtime.name()),
        ),
        Err(_) => row(
            "containers",
            "unsupported",
            "neither podman nor docker was found on the daemon's PATH (a missing dependency, not an OS limit)",
        ),
    }
}

/// Embedded terminals: pseudo-terminals are an OS facility.
fn terminals() -> FeatureSupport {
    if cfg!(unix) {
        row("terminal.pty", "supported", "POSIX pseudo-terminals")
    } else if cfg!(windows) {
        row("terminal.pty", "supported", "Windows ConPTY")
    } else {
        row(
            "terminal.pty",
            "unsupported",
            "this operating system has no pseudo-terminal facility",
        )
    }
}

/// The OS keychain secrets backend.
fn keychain() -> FeatureSupport {
    if cfg!(any(target_os = "macos", windows)) {
        return row("secrets.keychain", "supported", "the OS keychain");
    }
    if cfg!(target_os = "android") {
        return row(
            "secrets.keychain",
            "unsupported",
            "Android has no Secret Service keychain",
        );
    }
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
        row(
            "secrets.keychain",
            "supported",
            "the Secret Service over the D-Bus session bus",
        )
    } else {
        row(
            "secrets.keychain",
            "degraded",
            "no D-Bus session bus for the Secret Service (a missing service, not an OS limit); the secrets file is used",
        )
    }
}

/// Starting the daemon at login.
fn service_manager() -> FeatureSupport {
    if cfg!(target_os = "macos") {
        row("service.autostart", "supported", "launchd")
    } else if cfg!(windows) {
        row("service.autostart", "supported", "Task Scheduler")
    } else if on_path("systemctl").is_some() {
        row("service.autostart", "supported", "systemd user units")
    } else {
        row(
            "service.autostart",
            "unsupported",
            "systemctl was not found on the daemon's PATH (a missing dependency, not an OS limit)",
        )
    }
}

/// Every row, in a stable order.
pub(super) fn probe(home: &Path) -> Vec<FeatureSupport> {
    vec![
        sandbox(home),
        containers(),
        terminals(),
        keychain(),
        service_manager(),
    ]
}

#[cfg(test)]
mod tests {
    use super::probe;

    #[test]
    fn every_row_has_a_known_state_and_a_reason() {
        let home =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let rows = probe(home.path());
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].feature, "sandbox.local");
        for row in rows {
            assert!(
                ["supported", "degraded", "unsupported"].contains(&row.state.as_str()),
                "{row:?}"
            );
            assert!(!row.reason.is_empty(), "{row:?}");
        }
    }
}
