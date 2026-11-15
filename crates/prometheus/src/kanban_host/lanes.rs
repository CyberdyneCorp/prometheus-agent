//! External worker lanes (spec: kanban, Worker lanes). An enabled plugin
//! declares lanes under `[[kanban_lanes]]`; the dispatcher starts the
//! lane's broker (`prometheus kanban lane --run <id>`, [`super::broker`]),
//! which starts the lane `command` under the local sandbox, in the task's
//! workspace, with the run ticket on stdin (a program or argument naming a
//! file of the plugin's directory resolves there). The lane reports only
//! through `prometheus kanban ... --ticket -`, which hands the report to
//! the broker through the lane's mailbox; the broker checks the ticket and
//! applies it. Every claim still ends in one terminator or is reclaimed by
//! the liveness rules, exactly as a profile worker's.
//!
//! The sandbox grants the lane its workspace, its plugin's directory, the
//! capabilities its plugin was approved for, read access to the blob store
//! (the attachments its task lists) and write access to its mailbox only:
//! the board's database and the rest of the Prometheus home stay denied,
//! so a lane cannot touch another task, run or board.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};

use pmt_config::ProfileScope;
use pmt_config::grants::{GrantsFile, Subject, baseline};
use pmt_core::capability::{Capability, CapabilitySet};
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

/// What a lane runs for and where.
pub struct Start<'a> {
    /// The lane's name (the run's assignee).
    pub lane: &'a str,
    /// The board, task and run.
    pub board: &'a str,
    /// The task.
    pub task: &'a str,
    /// The run.
    pub run: &'a str,
    /// The task's workspace (the lane's working directory).
    pub workspace: &'a Path,
    /// The lane's mailbox (its only writable home path).
    pub mailbox: &'a Path,
    /// A worker's environment (`PATH`, the locale, the home).
    pub env: &'a [(String, OsString)],
    /// The user's home directory (for the sandbox's path rules).
    pub user_home: &'a Path,
}

/// Starts the lane of `start` under the sandbox with `ticket` on stdin and
/// the caller's output (the run's log); the child (in the caller's process
/// group, so stopping the run stops it too).
///
/// # Errors
///
/// Why it could not start (no such lane, the sandbox, the spawn).
pub fn spawn(scope: &ProfileScope, start: &Start<'_>, ticket: &str) -> Result<Child, String> {
    let lane = declared(scope)
        .into_iter()
        .find(|l| l.lane.name == start.lane)
        .ok_or_else(|| format!("no enabled plugin declares lane '{}'", start.lane))?;
    let home = scope.home();
    let paths = Paths {
        user_home: start.user_home.to_path_buf(),
        prometheus_home: home.to_path_buf(),
        workspace: start.workspace.to_path_buf(),
        scratch: vec![start.mailbox.to_path_buf()],
    };
    let program = program(&lane);
    let mut extra = vec![
        Capability::FsRead(format!("{}/**", lane.dir.display())),
        Capability::FsRead(format!("{}/**", home.join("kanban/blobs").display())),
    ];
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
    command
        .args(arguments(&lane))
        .current_dir(start.workspace)
        .env_clear()
        .envs(lane_env(start))
        .stdin(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start lane '{}': {e}", lane.lane.name))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(format!("{}\n", ticket.trim_end()).as_bytes())
            .map_err(|e| format!("cannot hand the lane its ticket: {e}"))?;
    }
    Ok(child)
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

/// The lane's environment: a worker's (`PATH`, the locale, the home),
/// which task and run it works (the ticket's secret comes only on stdin)
/// and its mailbox.
fn lane_env(start: &Start<'_>) -> Vec<(String, OsString)> {
    let mut env = start.env.to_vec();
    env.push(("PROMETHEUS_KANBAN_BOARD".to_owned(), start.board.into()));
    env.push(("PROMETHEUS_KANBAN_TASK".to_owned(), start.task.into()));
    env.push(("PROMETHEUS_KANBAN_RUN".to_owned(), start.run.into()));
    env.push((
        pmt_core::env::KANBAN_MAILBOX.to_owned(),
        start.mailbox.as_os_str().to_owned(),
    ));
    env
}
