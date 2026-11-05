//! One session's snapshots: the [`EditRecorder`] of its file tools and the
//! checkpoint hook of its terminal (spec: execution-environments, File
//! snapshots and undo; Snapshot triggers, caps and per-run toggle).
//!
//! A project is checkpointed at most once per turn, before the first
//! file-tool write in it or the first destructive command run in it.
//! Recording never fails or blocks the change it records: failures are
//! logged and the change goes ahead.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::checkpoint::{self, Trigger};
use super::destructive::is_destructive;
use super::journal::{self, Edit};
use super::store::{SnapshotStore, now_ms};
use super::tree::Tree;
use crate::files::edits::{EditCall, EditRecorder};

/// The session's current turn number (it only grows).
pub type TurnSource = Arc<dyn Fn() -> u64 + Send + Sync>;

/// `files.snapshot.*` as a session uses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Record anything at all (`files.snapshot.enabled`, or the run's
    /// `--checkpoints`/`--no-checkpoints`).
    pub enabled: bool,
    /// Checkpoint before destructive terminal commands
    /// (`files.snapshot.terminal` not `off`).
    pub terminal: bool,
    /// Larger files are listed as skipped.
    pub max_file_bytes: u64,
    /// Checkpoints kept per workspace.
    pub max_checkpoints: usize,
    /// Snapshot retention.
    pub retention: Duration,
    /// Store size cap.
    pub max_bytes: u64,
    /// Least time between automatic prunes.
    pub prune_interval: Duration,
}

/// One session's snapshot recorder.
pub struct SessionSnapshots {
    store: SnapshotStore,
    /// Where the session's files are.
    tree: Arc<dyn Tree>,
    session: String,
    workspace: PathBuf,
    settings: Settings,
    turn: TurnSource,
    /// Directories never checkpointed (Prometheus' own home).
    excluded: Vec<PathBuf>,
    /// The turn and the projects checkpointed in it.
    taken: Mutex<(u64, HashSet<PathBuf>)>,
}

impl std::fmt::Debug for SessionSnapshots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSnapshots")
            .field("session", &self.session)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl SessionSnapshots {
    /// The recorder of `session`, working in `workspace` (a path of
    /// `tree`); checkpoints never include `excluded` directories.
    #[must_use]
    pub fn new(
        store: SnapshotStore,
        tree: Arc<dyn Tree>,
        session: &str,
        workspace: &Path,
        settings: Settings,
        turn: TurnSource,
        excluded: Vec<PathBuf>,
    ) -> Self {
        Self {
            excluded,
            store,
            tree,
            session: session.to_owned(),
            workspace: workspace.to_path_buf(),
            settings,
            turn,
            taken: Mutex::new((0, HashSet::new())),
        }
    }

    /// The store it records into.
    #[must_use]
    pub fn store(&self) -> &SnapshotStore {
        &self.store
    }

    /// Where the session's files are.
    #[must_use]
    pub fn tree(&self) -> &dyn Tree {
        &*self.tree
    }

    /// The session's workspace, a path of [`SessionSnapshots::tree`].
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The session's current turn.
    #[must_use]
    pub fn turn(&self) -> u64 {
        (self.turn)()
    }

    /// Prunes the store when an automatic prune is due (never orphans).
    pub fn auto_prune(&self) {
        let result = super::manage::auto_prune(
            &self.store,
            self.settings.retention,
            self.settings.max_bytes,
            self.settings.prune_interval,
        );
        if let Err(err) = result {
            tracing::warn!("snapshot store prune failed: {err}");
        }
    }

    /// Before a terminal command in `cwd`: a checkpoint of its project
    /// when the command is destructive.
    pub fn before_command(&self, command: &str, cwd: &Path) {
        if self.settings.enabled && self.settings.terminal && is_destructive(command) {
            self.checkpoint(cwd, "terminal");
        }
    }

    /// Where the session's `path` is in its tree, or why it is not kept.
    fn locate(&self, path: &Path) -> Option<PathBuf> {
        let found = self.tree.locate(path);
        if found.is_none() {
            tracing::debug!(
                path = %path.display(),
                "not snapshotted: outside what this host can reach of the backend"
            );
        }
        found
    }

    /// Checkpoints the project of the session's `path` unless it already
    /// was this turn.
    fn checkpoint(&self, path: &Path, tool: &str) {
        let Some(path) = self.locate(path) else {
            return;
        };
        let root = checkpoint::project_of(&*self.tree, &path, &self.workspace);
        let turn = self.turn();
        {
            let mut taken = self.taken.lock().unwrap_or_else(PoisonError::into_inner);
            if taken.0 != turn {
                *taken = (turn, HashSet::new());
            }
            if !taken.1.insert(root.clone()) {
                return;
            }
        }
        let trigger = Trigger {
            excluded: &self.excluded,
            session: &self.session,
            turn,
            tool,
        };
        let taken = checkpoint::take(
            &self.store,
            &*self.tree,
            &root,
            &trigger,
            self.settings.max_file_bytes,
            self.settings.max_checkpoints,
        );
        match taken {
            Ok(cp) if !cp.skipped().is_empty() => tracing::info!(
                root = %root.display(),
                skipped = ?cp.skipped(),
                "checkpoint taken; files over files.snapshot.max_file_bytes skipped"
            ),
            Ok(_) => {}
            Err(err) => tracing::warn!(root = %root.display(), "checkpoint failed: {err}"),
        }
    }

    fn record(
        &self,
        path: &Path,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
        call: Option<&EditCall>,
    ) -> std::io::Result<()> {
        let before = before.map(|b| self.store.put(b)).transpose()?;
        let after = after.map(|b| self.store.put(b)).transpose()?;
        let Some(path) = self.locate(path) else {
            return Ok(());
        };
        let edit = Edit {
            turn: self.turn(),
            call_id: call.map(|c| c.call_id.clone()).unwrap_or_default(),
            tool: call.map_or_else(|| "file".to_owned(), |c| c.tool.clone()),
            path,
            backend: self.tree.backend().map(str::to_owned),
            before,
            after,
            at_ms: now_ms(),
        };
        journal::append(&self.store, &self.session, edit)
    }
}

impl EditRecorder for SessionSnapshots {
    fn before_change(&self, path: &Path, call: Option<&EditCall>) {
        if self.settings.enabled {
            self.checkpoint(path, call.map_or("file", |c| c.tool.as_str()));
        }
    }

    fn changed(
        &self,
        path: &Path,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
        call: Option<&EditCall>,
    ) {
        if !self.settings.enabled {
            return;
        }
        if let Err(err) = self.record(path, before, after, call) {
            tracing::warn!(path = %path.display(), "file snapshot not recorded: {err}");
        }
    }
}
