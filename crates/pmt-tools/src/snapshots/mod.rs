//! File snapshots, checkpoints, undo and rollback (spec:
//! execution-environments, File snapshots and undo; Snapshot triggers,
//! caps and per-run toggle; Rollback listing, diff and selective restore;
//! Snapshot store management CLI; Working-tree diff command).
//!
//! - [`store::SnapshotStore`]: the profile's content-addressed store of
//!   file contents, project checkpoints and session journals.
//! - [`session::SessionSnapshots`]: what a session records — every
//!   file-tool change (for `/undo`) and, once per project per turn, a
//!   checkpoint of the project (for `/rollback`).
//! - [`restore`]: undo, rollback listing, diff and restore.
//! - [`manage`]: status and pruning.
//! - [`worktree`]: the `/diff` command.
//! - [`tree::Tree`]: where the session's files are (this host, a mounted
//!   container workspace, or a backend's own filesystem).

pub mod checkpoint;
pub mod destructive;
pub mod journal;
pub mod manage;
pub mod restore;
pub mod session;
pub mod store;
pub mod tree;
pub mod worktree;

pub use session::{SessionSnapshots, Settings, TurnSource};
pub use store::SnapshotStore;
pub use tree::{BackendTree, HostTree, MountedTree, Tree};
