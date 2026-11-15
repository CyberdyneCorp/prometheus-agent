//! Kanban (spec: kanban): durable boards shared by every profile of a
//! home, driven by people from `prometheus kanban` and worked by profiles
//! the dispatcher starts as workers.
//!
//! - [`boards`]: storage at `<home>/kanban/boards/<slug>/`, slugs and
//!   selection (`--board`, `kanban.current_board`, `default`).
//! - [`model`]: statuses, actors, block kinds, workspaces, task kinds.
//! - [`workflow`]: the one typed table every transition is checked
//!   against (and the reference page is rendered from).
//! - [`board`]: an open board: creation, links, promotion, claims, blocks,
//!   schedules, completion gates, the review lane, archive, runs.
//! - [`lease`], [`dispatch`], [`process`], [`ticket`]: the single
//!   dispatcher per home, its tick, worker identity and run tickets.
//! - [`attach`], [`notify`], [`transfer`], [`context`]: attachments,
//!   notification subscriptions, export/import and the worker's context.

pub mod attach;
pub mod board;
pub mod boards;
pub mod context;
pub mod contract;
mod db;
pub mod dispatch;
pub mod lease;
pub mod model;
pub mod notify;
pub mod observe;
pub mod orchestrate;
pub mod process;
pub mod ticket;
pub mod transfer;
pub mod workflow;

pub use board::create::{Created, NewTask, Profiles};
pub use board::finish::{Completion, Handoff};
pub use board::flow::{Claim, ClaimOutcome, Role};
pub use board::runs::{Breaker, RunEnd};
pub use board::{Board, Edit, Filter};
pub use boards::{BoardSettings, Boards, DEFAULT_BOARD};
pub use model::{Actor, ActorKind, BlockKind, Run, Status, Task, TaskKind, Workspace, now_ms};
