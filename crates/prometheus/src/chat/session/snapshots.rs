//! A session's file snapshots (spec: execution-environments, File
//! snapshots and undo; Snapshot triggers, caps and per-run toggle): built
//! from `files.snapshot` and the run's `--checkpoints`/`--no-checkpoints`,
//! keyed by the session's turn (its user message count in the store), on
//! whatever backend the session's files are: this host, a container whose
//! workspace is a host bind mount (read and restored on the host), or a
//! remote backend's own filesystem (through its file backend).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::SnapshotTerminal;
use pmt_core::id::SessionId;
use pmt_store::Store;
use pmt_tools::snapshots::worktree::{BackendGit, Worktree};
use pmt_tools::snapshots::{
    BackendTree, HostTree, MountedTree, SessionSnapshots, Settings, SnapshotStore, Tree,
};

use super::backend::Remote;

/// Opens a session's snapshots once its backend is known; `None` from
/// [`opener`] when snapshots are off.
pub struct Opener<'a> {
    scope: &'a ProfileScope,
    store: &'a Arc<Store>,
    session: SessionId,
}

/// The opener of `session`'s snapshots; `None` when snapshots are off
/// (`files.snapshot.enabled = false`, which `chat --checkpoints` /
/// `--no-checkpoints` override for one run).
#[must_use]
pub fn opener<'a>(
    scope: &'a ProfileScope,
    store: &'a Arc<Store>,
    session: SessionId,
) -> Option<Opener<'a>> {
    scope.config().files.snapshot.enabled.then_some(Opener {
        scope,
        store,
        session,
    })
}

impl Opener<'_> {
    /// The recorder of the session working in host directory `cwd`, or on
    /// `remote` when it has a non-local backend. An automatic prune of the
    /// store starts in the background when due.
    #[must_use]
    pub fn open(&self, cwd: &Path, remote: Option<&Remote>) -> Arc<SessionSnapshots> {
        let config = &self.scope.config().files.snapshot;
        let settings = Settings {
            enabled: true,
            terminal: config.terminal != SnapshotTerminal::Off,
            max_file_bytes: config.max_file_bytes,
            max_checkpoints: usize::try_from(config.max_checkpoints).unwrap_or(usize::MAX),
            retention: Duration::from_secs(config.retention_secs),
            max_bytes: config.max_bytes,
            prune_interval: Duration::from_secs(config.prune_interval_secs),
        };
        let (tree, workspace) = tree_of(self.scope, cwd, remote);
        let turns = Arc::clone(self.store);
        let session = self.session;
        let turn = Arc::new(move || turns.user_turns(session).unwrap_or_default());
        let snapshots = Arc::new(SessionSnapshots::new(
            SnapshotStore::of_profile(self.scope.dir()),
            tree,
            &session.to_string(),
            &workspace,
            settings,
            turn,
            vec![self.scope.home().to_path_buf()],
        ));
        let pruning = Arc::clone(&snapshots);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(move || pruning.auto_prune());
        }
        snapshots
    }
}

/// Where `/diff` of the session working in host directory `cwd`, or on
/// `remote`, runs git: on this host for a local session or a mounted
/// container workspace (its host directory), else on the backend at the
/// session's start directory there.
#[must_use]
pub fn worktree(cwd: &Path, remote: Option<&Remote>) -> Worktree {
    let Some(remote) = remote else {
        return Worktree::host(cwd);
    };
    match mounted(remote) {
        Some((_, workspace)) => Worktree::host(&workspace),
        None => Worktree {
            git: Arc::new(BackendGit(Arc::clone(&remote.exec))),
            cwd: remote.start_cwd.clone(),
        },
    }
}

/// Where the session's files are and its workspace there (for a mounted
/// container workspace, the host directory).
fn tree_of(scope: &ProfileScope, cwd: &Path, remote: Option<&Remote>) -> (Arc<dyn Tree>, PathBuf) {
    let Some(remote) = remote else {
        return (Arc::new(HostTree), cwd.to_path_buf());
    };
    match mounted(remote) {
        Some((tree, workspace)) => (Arc::new(tree), workspace),
        None => (
            Arc::new(BackendTree::new(
                &scope.config().terminal.backend,
                Arc::clone(&remote.files),
            )),
            remote.start_cwd.clone(),
        ),
    }
}

/// A container session whose workspace is a host bind mount: the mount
/// and the session's start directory located on the host.
fn mounted(remote: &Remote) -> Option<(MountedTree, PathBuf)> {
    let host = remote
        .container
        .as_ref()
        .and_then(|c| c.container().spec().workspace.clone())?;
    let tree = MountedTree::new(Path::new(pmt_sandbox::container::WORKSPACE), &host);
    let workspace = tree.locate(&remote.start_cwd).unwrap_or(host);
    Some((tree, workspace))
}
