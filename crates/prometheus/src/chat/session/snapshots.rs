//! A session's file snapshots (spec: execution-environments, File
//! snapshots and undo; Snapshot triggers, caps and per-run toggle): built
//! from `files.snapshot` and the run's `--checkpoints`/`--no-checkpoints`,
//! keyed by the session's turn (its user message count in the store).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::SnapshotTerminal;
use pmt_core::id::SessionId;
use pmt_store::Store;
use pmt_tools::snapshots::{SessionSnapshots, Settings, SnapshotStore};

/// The recorder of `session` working in `cwd`; `None` when snapshots are
/// off (`files.snapshot.enabled = false`, which `chat --checkpoints` /
/// `--no-checkpoints` override for one run). An
/// automatic prune of the store starts in the background when due.
#[must_use]
pub fn open(
    scope: &ProfileScope,
    store: &Arc<Store>,
    session: SessionId,
    cwd: &Path,
) -> Option<Arc<SessionSnapshots>> {
    let config = &scope.config().files.snapshot;
    if !config.enabled {
        return None;
    }
    let settings = Settings {
        enabled: true,
        terminal: config.terminal != SnapshotTerminal::Off,
        max_file_bytes: config.max_file_bytes,
        max_checkpoints: usize::try_from(config.max_checkpoints).unwrap_or(usize::MAX),
        retention: Duration::from_secs(config.retention_secs),
        max_bytes: config.max_bytes,
        prune_interval: Duration::from_secs(config.prune_interval_secs),
    };
    let turns = Arc::clone(store);
    let turn = Arc::new(move || turns.user_turns(session).unwrap_or_default());
    let snapshots = Arc::new(SessionSnapshots::new(
        SnapshotStore::of_profile(scope.dir()),
        &session.to_string(),
        cwd,
        settings,
        turn,
        vec![scope.home().to_path_buf()],
    ));
    let pruning = Arc::clone(&snapshots);
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn_blocking(move || pruning.auto_prune());
    }
    Some(snapshots)
}
