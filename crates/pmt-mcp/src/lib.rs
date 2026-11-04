//! MCP for Prometheus (design D7; spec: plugin-system, MCP client servers;
//! MCP server mode; tool-system, MCP tool naming and filtering; MCP runtime
//! safety).
//!
//! The client side connects the external servers a profile configures
//! (`[mcp.servers.<name>]`): [`connection`] starts one (a sandboxed child
//! process speaking stdio, or a streamable HTTP endpoint) and lists its
//! tools, [`server`] keeps one server's connection, restart backoff and
//! circuit breaker, [`manager`] holds a profile's servers and registers
//! their tools ([`tool`], named by [`naming`]) in its tool registry. The
//! server side, [`serve`], offers Prometheus tools and skills to MCP
//! clients. Every connection runs on one process-wide runtime
//! ([`runtime`]), so it outlives the runtime of the command or session
//! that started it.

pub mod cache;
pub mod connection;
pub mod costs;
pub mod handler;
pub mod host;
pub mod manager;
pub mod naming;
pub mod requests;
pub mod result;
pub mod runtime;
pub mod sanitize;
pub mod serve;
pub mod server;
pub mod tool;
pub mod utility;

#[cfg(feature = "fixture")]
pub mod fixture;
#[cfg(feature = "testkit")]
pub mod testkit;

pub use connection::Launch;
pub use host::{ClientHost, NoHost};
pub use manager::{Manager, ServerEntry, ServerStatus};
pub use server::{Server, Settings};
