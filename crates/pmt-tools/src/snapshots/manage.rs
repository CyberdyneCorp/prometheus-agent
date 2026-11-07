//! Snapshot store management (spec: execution-environments, Snapshot store
//! management CLI): status, pruning by age and size, and the automatic
//! background prune, which never deletes orphans (a workspace whose
//! directory is missing may be on an unmounted volume).

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use super::checkpoint;
use super::store::{SnapshotStore, dir_size, now_ms};

/// One workspace of the status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceStatus {
    /// The project's root.
    pub root: PathBuf,
    /// Bytes of the objects its checkpoints name (shared ones counted in
    /// each) plus its manifests.
    pub bytes: u64,
    /// Its checkpoints.
    pub checkpoints: usize,
    /// Last use, Unix milliseconds.
    pub last_used_ms: i64,
    /// Its directory no longer exists.
    pub orphan: bool,
}

/// The store's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    /// Bytes under the store.
    pub total_bytes: u64,
    /// Workspaces in the store.
    pub workspace_count: usize,
    /// The most recently used ones, at most the requested number.
    pub workspaces: Vec<WorkspaceStatus>,
}

/// The status, with at most `limit` workspaces listed.
#[must_use]
pub fn status(store: &SnapshotStore, limit: usize) -> Status {
    let all = store.workspaces();
    let workspaces = all
        .iter()
        .take(limit)
        .map(|(id, meta)| {
            let checkpoints = checkpoint::list(store, id);
            let mut objects: Vec<&str> = checkpoints
                .iter()
                .flat_map(|cp| cp.files.iter().filter_map(|f| f.hash.as_deref()))
                .collect();
            objects.sort_unstable();
            objects.dedup();
            let bytes = objects
                .iter()
                .map(|h| {
                    fs::metadata(store.root().join("objects").join(&h[..2]).join(h))
                        .map_or(0, |m| m.len())
                })
                .sum::<u64>()
                + dir_size(&store.workspace_dir(id));
            WorkspaceStatus {
                root: meta.root.clone(),
                bytes,
                checkpoints: checkpoints.len(),
                last_used_ms: meta.last_used_ms,
                orphan: !meta.root.is_dir(),
            }
        })
        .collect();
    Status {
        total_bytes: store.size(),
        workspace_count: all.len(),
        workspaces,
    }
}

/// How a prune runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrunePolicy {
    /// Workspaces (and journals) unused longer than this are deleted.
    pub retention: Duration,
    /// The store is cut down to this many bytes, oldest checkpoints first.
    pub max_bytes: u64,
    /// Delete workspaces whose directory is missing.
    pub orphans: bool,
}

/// What a prune deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Pruned {
    /// Workspaces deleted.
    pub workspaces: Vec<PathBuf>,
    /// Checkpoints deleted for the size cap.
    pub checkpoints: usize,
    /// Session journals deleted.
    pub journals: usize,
    /// Bytes freed.
    pub bytes: u64,
}

/// Prunes the store under `policy`.
///
/// # Errors
///
/// The store cannot be read or written.
pub fn prune(store: &SnapshotStore, policy: PrunePolicy) -> io::Result<Pruned> {
    let before = store.size();
    let cutoff = now_ms() - i64::try_from(policy.retention.as_millis()).unwrap_or(i64::MAX);
    let mut pruned = Pruned::default();
    for (id, meta) in store.workspaces() {
        let orphan = !meta.root.is_dir();
        // An orphan is only ever deleted on request, whatever its age.
        let expired = meta.last_used_ms < cutoff && !orphan;
        if expired || (orphan && policy.orphans) {
            fs::remove_dir_all(store.workspace_dir(&id))?;
            pruned.workspaces.push(meta.root);
        }
    }
    for journal in store.journals() {
        let old = fs::metadata(&journal)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > policy.retention);
        if old {
            fs::remove_file(&journal)?;
            pruned.journals += 1;
        }
    }
    store.collect_garbage()?;
    pruned.checkpoints = cap(store, policy.max_bytes)?;
    pruned.bytes = before.saturating_sub(store.size());
    Ok(pruned)
}

/// Deletes the oldest checkpoints (never an orphan's) until the store
/// fits `max_bytes`.
fn cap(store: &SnapshotStore, max_bytes: u64) -> io::Result<usize> {
    let mut deleted = 0;
    while store.size() > max_bytes {
        let oldest = store
            .workspaces()
            .into_iter()
            .filter(|(_, meta)| meta.root.is_dir())
            .flat_map(|(id, _)| {
                checkpoint::list(store, &id)
                    .into_iter()
                    .map(move |cp| (cp.id.clone(), id.clone()))
            })
            .min();
        let Some((cp, id)) = oldest else { break };
        fs::remove_file(
            store
                .workspace_dir(&id)
                .join("checkpoints")
                .join(format!("{cp}.json")),
        )?;
        store.collect_garbage()?;
        deleted += 1;
    }
    Ok(deleted)
}

/// The automatic prune: at most once per `interval` (recorded in the
/// store), never deleting orphans. Returns `None` when it was not due.
///
/// # Errors
///
/// The store cannot be read or written.
pub fn auto_prune(
    store: &SnapshotStore,
    retention: Duration,
    max_bytes: u64,
    interval: Duration,
) -> io::Result<Option<Pruned>> {
    let stamp = store.root().join("last_prune");
    let due = fs::read_to_string(&stamp)
        .ok()
        .and_then(|t| t.trim().parse::<i64>().ok())
        .is_none_or(|at| now_ms() - at >= i64::try_from(interval.as_millis()).unwrap_or(i64::MAX));
    if !due || !store.root().is_dir() {
        return Ok(None);
    }
    fs::write(&stamp, now_ms().to_string())?;
    prune(
        store,
        PrunePolicy {
            retention,
            max_bytes,
            orphans: false,
        },
    )
    .map(Some)
}
