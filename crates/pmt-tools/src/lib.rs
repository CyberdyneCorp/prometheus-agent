//! Built-in tools, tool descriptors (effect, idempotency, resource claims,
//! capabilities), toolsets and the tool registry.
//!
//! So far: [`protected`], the instruction files whose agent writes always
//! need human approval, and [`terminal`], the execution environment behind
//! the `terminal` and `process` tools.

pub mod protected;
pub mod terminal;
