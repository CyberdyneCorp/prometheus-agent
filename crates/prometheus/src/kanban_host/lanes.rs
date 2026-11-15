//! External worker lanes (spec: kanban, Worker lanes). An enabled plugin
//! declares lanes under `[[kanban_lanes]]`; the dispatcher starts a lane
//! task's lane `command` under the local sandbox, in the task's workspace,
//! with the run ticket on stdin (a program or argument naming a file of the
//! plugin's directory resolves there). The lane reports only through
//! `prometheus kanban ... --ticket -` (the ticket authenticates it as the
//! run's worker) and every claim still ends in one terminator or is
//! reclaimed by the liveness rules, exactly as a profile worker's.
//!
//! The sandbox grants the lane its workspace, its plugin's directory, the
//! capabilities its plugin was approved for, and its board's directory and
//! the blob store (so its `prometheus kanban` calls can write the board);
//! the rest of the Prometheus home stays denied.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use pmt_config::ProfileScope;
use pmt_config::grants::{GrantsFile, Subject, baseline};
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_kanban::dispatch::SpawnRequest;
use pmt_plugins::load::State;
use pmt_plugins::manifest::lanes::Lane;
use pmt_sandbox::{Paths, Policy, Sandbox};

/// A lane an enabled plugin declares.
#[derive(Debug, Clone)]
pub struct Declared {
    /// The plugin.
    pub plugin: String,
    /// Its directory.
    pub dir: PathBuf,
    /// Its content digest (its grants are bound to it).
    pub digest: Option<String>,
    /// The lane.
    pub lane: Lane,
}

/// The lanes of `scope`'s enabled, loadable plugins.
#[must_use]
pub fn declared(scope: &ProfileScope) -> Vec<Declared> {
    let version = crate::plugins_host::running_version();
    let blocked = crate::plugins_host::catalog::kill_list(scope);
    let cwd = scope.dir().to_path_buf();
    let inputs = crate::plugins_host::inputs(scope, &cwd, &version, &blocked);
    let mut out = Vec::new();
    for (plugin, status) in pmt_plugins::load::survey(&inputs) {
        let (Ok(manifest), Some(dir)) = (&plugin.manifest, &plugin.dir) else {
            continue;
        };
        if status.state != State::Loaded {
            continue;
        }
        out.extend(manifest.kanban_lanes.iter().map(|lane| Declared {
            plugin: manifest.id.clone(),
            dir: dir.clone(),
            digest: plugin.digest.clone(),
            lane: lane.clone(),
        }));
    }
    out
}

/// The capabilities `plugin` was approved for.
fn approved(scope: &ProfileScope, lane: &Declared) -> CapabilitySet {
    let Some(digest) = lane.digest.clone() else {
        return CapabilitySet::default();
    };
    GrantsFile::of(scope.profile())
        .load()
        .map(|g| {
            g.capabilities(&Subject::Plugin {
                id: lane.plugin.clone(),
                digest,
            })
        })
        .unwrap_or_default()
}

/// Starts `request`'s lane in `workspace`; returns the pid and start time.
///
/// # Errors
///
/// Why it could not start (no such lane, the sandbox, the spawn).
pub fn spawn(
    scope: &ProfileScope,
    request: &SpawnRequest<'_>,
    (workspace, env, user_home): (&Path, &[(String, OsString)], &Path),
    log: std::fs::File,
) -> Result<(u32, u64), String> {
    let lane = declared(scope)
        .into_iter()
        .find(|l| l.lane.name == request.profile)
        .ok_or_else(|| format!("no enabled plugin declares lane '{}'", request.profile))?;
    let home = scope.home();
    let board_dir = home.join("kanban/boards").join(request.board);
    let paths = Paths {
        user_home: user_home.to_path_buf(),
        prometheus_home: home.to_path_buf(),
        workspace: workspace.to_path_buf(),
        scratch: vec![board_dir, home.join("kanban/blobs")],
    };
    let program = program(&lane);
    let mut extra = vec![Capability::FsRead(format!("{}/**", lane.dir.display()))];
    if let Some(dir) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::parent)
    {
        extra.push(Capability::FsRead(format!("{}/**", dir.display())));
    }
    let capabilities = baseline(&scope.config().security)
        .union(&approved(scope, &lane))
        .union(&extra.into_iter().collect());
    let sandbox = Sandbox::new(
        Policy::from_set(&capabilities, &paths),
        &scope.config().security.sandbox,
    )
    .map_err(|e| e.message().to_owned())?;
    let mut command = sandbox
        .command(&program)
        .map_err(|e| e.message().to_owned())?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    command
        .args(arguments(&lane))
        .current_dir(workspace)
        .env_clear()
        .envs(lane_env(env, request))
        .stdin(Stdio::piped())
        .stdout(log)
        .stderr(err);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start lane '{}': {e}", lane.lane.name))?;
    let pid = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        let ticket_line = format!("{}\n", request.ticket.line());
        stdin
            .write_all(ticket_line.as_bytes())
            .map_err(|e| format!("cannot hand the lane its ticket: {e}"))?;
    }
    let start = super::spawn::start_time(pid);
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    start
        .ok_or_else(|| "the lane exited before it could be identified".to_owned())
        .map(|start| (pid, start))
}

/// The lane's arguments: one naming a file of its plugin directory (a
/// script) becomes that file's path, since the lane runs in the task's
/// workspace.
fn arguments(lane: &Declared) -> Vec<std::ffi::OsString> {
    lane.lane.command[1..]
        .iter()
        .map(|arg| {
            let local = lane.dir.join(arg);
            if Path::new(arg).is_relative() && local.is_file() {
                local.into_os_string()
            } else {
                arg.into()
            }
        })
        .collect()
}

/// The lane's program: relative to its plugin directory when it names a
/// file there, else looked up on `PATH`.
fn program(lane: &Declared) -> PathBuf {
    let named = PathBuf::from(&lane.lane.command[0]);
    let local = lane.dir.join(&named);
    if named.is_relative() && local.is_file() {
        local
    } else {
        named
    }
}

/// The lane's environment: a worker's (`PATH`, the locale, the home) and
/// which task and run it works (the ticket's secret comes only on stdin).
fn lane_env(base: &[(String, OsString)], request: &SpawnRequest<'_>) -> Vec<(String, OsString)> {
    let mut env = base.to_vec();
    env.push(("PROMETHEUS_KANBAN_BOARD".to_owned(), request.board.into()));
    env.push((
        "PROMETHEUS_KANBAN_TASK".to_owned(),
        request.task.id.clone().into(),
    ));
    env.push(("PROMETHEUS_KANBAN_RUN".to_owned(), request.run_id.into()));
    env
}
