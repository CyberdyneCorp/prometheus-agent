//! Git worktrees for sessions and subagents (spec: git-worktrees).
//!
//! [`create`] makes a tree under `<repo>/.worktrees/<name>` on branch
//! `prometheus/<name>` without running anything the repository names
//! ([`git`] pins hooks, fsmonitor and filters off); [`tree`] lists the
//! trees Prometheus made and their live owners; [`inspect`] reads what a
//! tree holds; [`archive`] saves a tree's whole state to
//! `refs/prometheus/archive/<name>` and restores it; [`sweep`] applies the
//! stale-tree decision table; [`registry`] remembers the repositories to
//! sweep; [`exit`] decides what a session's end does to its tree.

pub mod archive;
pub mod create;
pub mod exit;
pub mod git;
pub mod inspect;
pub mod names;
pub mod registry;
pub mod repo;
pub mod sweep;
pub mod tree;

pub use names::Kind;
pub use repo::Repo;
pub use tree::Tree;
