//! Whether this process runs under a supervisor that starts it again
//! after it exits 75 (a gateway daemon restart or update handover): the
//! container init (`prometheus` as PID 1, which marks its child with
//! [`SUPERVISOR_ENV`]), a systemd service (`INVOCATION_ID`) or a launchd
//! job (`XPC_SERVICE_NAME`, which an interactive macOS shell sets to `0`).
//! A process identity fact read from the environment its supervisor set,
//! not configuration.
//!
//! systemd and launchd pass their marker on to every descendant: a terminal
//! or CI runner that is itself a service (GNOME Terminal, a GitHub Actions
//! runner, an app launchd started) hands it to its shell and on to a
//! foreground `prometheus daemon run`, which no manager restarts. Their
//! markers therefore count only when the service manager itself started
//! this process (it is the parent).

use std::sync::OnceLock;

/// Set by the container init for the command it runs.
pub const SUPERVISOR_ENV: &str = "PMT_SUPERVISOR";

/// The service managers' markers, inherited by every descendant.
const MANAGER_MARKERS: &[&str] = &["INVOCATION_ID", "XPC_SERVICE_NAME"];

/// The variables that mark a supervised process. A process that starts a
/// daemon of its own (`prometheus daemon start`) removes them from it: a
/// shell or CI job running inside a systemd service (`INVOCATION_ID`)
/// passes them on, yet nothing would restart that detached daemon.
pub const MARKERS: &[&str] = &[SUPERVISOR_ENV, "INVOCATION_ID", "XPC_SERVICE_NAME"];

/// Whether a supervisor restarts this process after exit 75. Decided once,
/// on the first call: the daemon asks at startup, while the process that
/// started it is still its parent.
#[must_use]
pub fn supervised() -> bool {
    static SUPERVISED: OnceLock<bool> = OnceLock::new();
    *SUPERVISED.get_or_init(|| decide(marked, parent_is_service_manager))
}

/// Whether `name` is set to a value that marks supervision.
fn marked(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

/// The container init's own marker always counts; a service manager's
/// only when that manager is the parent.
fn decide(marked: impl Fn(&str) -> bool, parent_is_manager: impl FnOnce() -> bool) -> bool {
    marked(SUPERVISOR_ENV) || (MANAGER_MARKERS.iter().any(|m| marked(m)) && parent_is_manager())
}

/// systemd runs a system service as a child of PID 1 and a user service
/// as a child of the `systemd --user` manager.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn parent_is_service_manager() -> bool {
    let parent = std::os::unix::process::parent_id();
    parent == 1
        || std::fs::read_to_string(format!("/proc/{parent}/comm"))
            .is_ok_and(|comm| comm.trim_end() == "systemd")
}

/// launchd (PID 1) starts every job, system daemon and user agent alike.
#[cfg(target_os = "macos")]
fn parent_is_service_manager() -> bool {
    std::os::unix::process::parent_id() == 1
}

/// Elsewhere no supervisor Prometheus installs sets these markers, so the
/// marker alone decides.
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn parent_is_service_manager() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(names: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |name| names.contains(&name)
    }

    #[test]
    fn the_container_init_marker_counts_whoever_the_parent_is() {
        assert!(decide(only(&[SUPERVISOR_ENV]), || false));
    }

    #[test]
    fn an_inherited_service_marker_does_not_supervise_a_shell_child() {
        assert!(!decide(only(&["INVOCATION_ID"]), || false));
        assert!(!decide(only(&["XPC_SERVICE_NAME"]), || false));
    }

    #[test]
    fn a_service_marker_counts_when_the_manager_started_the_process() {
        assert!(decide(only(&["INVOCATION_ID"]), || true));
        assert!(decide(only(&["XPC_SERVICE_NAME"]), || true));
        assert!(!decide(only(&[]), || true));
    }
}
