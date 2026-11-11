//! Whether this process runs under a supervisor that starts it again
//! after it exits 75 (a gateway daemon restart or update handover): the
//! container init (`prometheus` as PID 1, which marks its child with
//! [`SUPERVISOR_ENV`]), a systemd service (`INVOCATION_ID`) or a launchd
//! job (`XPC_SERVICE_NAME`, which an interactive macOS shell sets to `0`).
//! A process identity fact read from the environment its supervisor set,
//! not configuration.

/// Set by the container init for the command it runs.
pub const SUPERVISOR_ENV: &str = "PMT_SUPERVISOR";

/// Whether a supervisor restarts this process after exit 75.
#[must_use]
pub fn supervised() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0");
    set(SUPERVISOR_ENV) || set("INVOCATION_ID") || set("XPC_SERVICE_NAME")
}
