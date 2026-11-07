//! Session automation (spec: session-automation): one standing `/goal`
//! driven by an auxiliary judge, a timer-driven `/loop` and an idle-only
//! `/heartbeat` per session.
//!
//! This crate is the host-agnostic part: what the hosts (the line
//! interface, the daemon and the messaging gateway) share so every surface
//! behaves the same.
//!
//! - [`state`]: the persisted per-session state of each kind.
//! - [`command`]: the `/goal`, `/subgoal`, `/loop` and `/heartbeat`
//!   grammar; [`contract`] the inline completion contract.
//! - [`judge`]: the `goal_judge` prompts and the verdict contract.
//! - [`goal`]: what a turn boundary does to a goal (gates, verdicts,
//!   budget, wait barriers).
//! - [`cadence`]: loop and heartbeat timing.
//! - [`arbiter`]: which automated turn, if any, an idle boundary admits.
//!
//! Every automated wakeup is a plain user turn: nothing here touches the
//! system prompt, tools or history, so the prompt cache prefix holds.

pub mod arbiter;
pub mod cadence;
pub mod command;
pub mod contract;
pub mod duration;
pub mod goal;
pub mod judge;
pub mod state;

pub use state::{Automation, Goal, GoalStatus, Heartbeat, Kind, Limits, Loop, RunStatus};
