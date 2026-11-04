//! What identifies the daemon to the self-termination guard (spec:
//! tool-system, Self-termination guard): its pids (this process inside
//! the daemon, plus the one `run/gateway.json` names), the executable's
//! name and the supervisor's service names.

use std::path::Path;

use pmt_core::runtime::{Role, live_record};
use pmt_tools::approval::selfterm::Daemon;

use super::service::SERVICE_NAMES;

/// The base name every Prometheus executable has.
const EXECUTABLE: &str = "prometheus";

/// The daemon of `home`; `inside` when this process is that daemon
/// (`approvals test` looks at it from beside it).
#[must_use]
pub(crate) fn daemon(home: &Path, inside: bool) -> Daemon {
    let mut pids = if inside {
        vec![std::process::id()]
    } else {
        Vec::new()
    };
    match live_record(home, Role::Daemon) {
        Ok(Some(record)) if !pids.contains(&record.pid) => pids.push(record.pid),
        Ok(_) => {}
        Err(err) => tracing::debug!(error = %err, "daemon record unreadable"),
    }
    let mut executables = vec![EXECUTABLE.to_owned()];
    let own = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_stem().map(|s| s.to_string_lossy().to_lowercase()));
    if let Some(own) = own.filter(|own| !executables.contains(own)) {
        executables.push(own);
    }
    Daemon {
        pids,
        executables,
        services: SERVICE_NAMES.iter().map(|&s| s.to_owned()).collect(),
    }
}
