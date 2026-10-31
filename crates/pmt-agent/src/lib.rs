//! The agent turn loop, system-prompt assembly and context compression.
//!
//! So far: [`tools`], the agent-level tools the turn loop executes itself
//! (`todo`, `clarify`, `session_search`), and [`vision`], how images reach
//! the model.

pub mod tools;
pub mod vision;
