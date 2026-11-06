//! The plugin host (spec: plugin-system; design D7): `plugin.toml`
//! manifests, discovery and opt-in enablement, digest-bound capability
//! grants, the WIT worlds and the wasmtime component runtime that runs
//! `wasm` plugins with only the capabilities the user approved.

pub mod backend;
pub mod catalog;
pub mod digest;
pub mod discovery;
pub mod events;
pub mod extensions;
pub mod grants;
pub mod hooks;
pub mod host;
pub mod install;
pub mod load;
pub mod manifest;
pub mod mcp_catalog;
pub mod package;
pub mod providers;
pub mod python;
pub mod record;
pub mod routers;
pub mod surfaces;
pub mod terminal;
#[cfg(feature = "testkit")]
pub mod testkit;
pub mod tools;
pub mod worlds;
