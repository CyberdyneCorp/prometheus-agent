//! Built-in tools, tool descriptors (effect, idempotency, resource claims,
//! capabilities), toolsets and the tool registry.
//!
//! The tool system: [`descriptor`] (what a tool declares), [`registry`]
//! (one profile's tools and toolsets over the shared built-ins),
//! [`toolset`] and [`selection`] (which tools a session selects),
//! [`probe`] and [`surface`] (what a session exposes, frozen at start),
//! [`dispatch`] (the call pipeline and batches), [`args`], [`sanitize`],
//! [`budget`] and [`spillover`] (pipeline steps) and [`result`] (the typed
//! result). Also [`approval`], the command approval gate, [`protected`], the instruction files whose agent writes
//! always need human approval, [`grants`], the capability grant gate, and
//! [`terminal`], the execution environment behind the `terminal` and
//! `process` tools.
//!
//! The built-in tools: [`files`] (`read_file`, `write_file`, `patch`,
//! `search_files`), [`terminal::tool`] (`terminal`, `process`) and
//! [`web`] (`web_search`, `web_extract`); [`builtin_tools`] lists them and
//! [`environments`] is where their handlers find each session's state.

pub mod approval;
pub mod args;
pub mod budget;
pub mod descriptor;
pub mod dispatch;
pub mod environments;
pub mod files;
pub mod grants;
pub mod probe;
pub mod protected;
pub mod registry;
pub mod result;
pub mod sanitize;
pub mod selection;
pub mod spillover;
pub mod surface;
pub mod terminal;
pub mod toolset;
pub mod web;

#[cfg(test)]
mod testing;

pub use descriptor::{
    CallContext, Effect, Executor, Owner, PathClaim, RegisteredTool, ResourceClaims,
    ToolDescriptor, ToolFailure, ToolHandler,
};
pub use dispatch::{DispatchSetup, Dispatcher, ToolCall};
pub use registry::{Builtins, ToolRegistry};
pub use result::{Content, Status, ToolResult};
pub use selection::Selection;
pub use surface::{Exclusion, SurfaceInputs, ToolSurface};
pub use toolset::Toolset;

use std::sync::Arc;

use environments::Environments;

/// The built-in tools implemented by this crate, their handlers bound to
/// `environments`.
#[must_use]
pub fn builtin_tools(environments: &Arc<Environments>) -> Vec<RegisteredTool> {
    let mut tools = web::tools(environments);
    tools.extend(terminal::tool::tools(environments));
    tools.extend(files::tools::tools(environments));
    tools
}
