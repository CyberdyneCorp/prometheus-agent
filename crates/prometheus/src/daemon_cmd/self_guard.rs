//! What identifies the daemon to the self-termination guard (spec:
//! tool-system, Self-termination guard): its pids (this process inside
//! the daemon, plus the one `run/gateway.json` names), their command
//! lines (what `pkill -f` matches), the executable's name, the
//! supervisor's service names and the spellings of its `run/` directory
//! (where its pid record lives).

use std::path::Path;

use pmt_core::runtime::{Role, live_record, process_command_line, run_dir};
use pmt_tools::approval::selfterm::Daemon;

use super::service::SERVICE_NAMES;

/// The base name every Prometheus executable has.
const EXECUTABLE: &str = "prometheus";

/// Spellings of a run directory that do not depend on where the home is:
/// through the home variable, and the default home's (`~/.prometheus`),
/// which a model reaches for first.
const RUN_SPELLINGS: &[&str] = &[
    "$PROMETHEUS_HOME/run",
    "${PROMETHEUS_HOME}/run",
    ".prometheus/run",
];

/// The daemon of `home`; `inside` when this process is that daemon
/// (`approvals test` looks at it from beside it). `user_home` lets the
/// guard recognise `~/...` spellings of the run directory.
#[must_use]
pub(crate) fn daemon(home: &Path, user_home: Option<&Path>, inside: bool) -> Daemon {
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
    let exe = std::env::current_exe().ok();
    let own = exe
        .as_ref()
        .and_then(|exe| exe.file_stem().map(|s| s.to_string_lossy().to_lowercase()));
    if let Some(own) = own.filter(|own| !executables.contains(own)) {
        executables.push(own);
    }
    let mut command_lines: Vec<String> = pids
        .iter()
        .filter_map(|&pid| process_command_line(pid))
        .collect();
    // The daemon usually runs this same binary: its path is part of the
    // daemon's command line even when the process cannot be read.
    if let Some(exe) = exe {
        command_lines.push(exe.display().to_string());
    }
    Daemon {
        pids,
        executables,
        command_lines,
        services: SERVICE_NAMES.iter().map(|&s| s.to_owned()).collect(),
        run_dirs: run_dirs(home, user_home),
    }
}

/// The absolute run directory, its home-relative tail (so `~/<tail>` and
/// `$HOME/<tail>` match as substrings) and [`RUN_SPELLINGS`].
fn run_dirs(home: &Path, user_home: Option<&Path>) -> Vec<String> {
    let run = run_dir(home);
    let mut dirs = vec![run.display().to_string()];
    if let Some(tail) = user_home.and_then(|user| run.strip_prefix(user).ok()) {
        dirs.push(tail.display().to_string());
    }
    for spelling in RUN_SPELLINGS {
        if !dirs.iter().any(|d| d == spelling) {
            dirs.push((*spelling).to_owned());
        }
    }
    dirs
}
