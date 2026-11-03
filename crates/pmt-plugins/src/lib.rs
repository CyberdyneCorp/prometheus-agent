//! The plugin host (spec: plugin-system; design D7): `plugin.toml`
//! manifests, discovery and opt-in enablement, digest-bound capability
//! grants, the WIT worlds and the wasmtime component runtime that runs
//! `wasm` plugins with only the capabilities the user approved.

pub mod digest;
pub mod discovery;
pub mod grants;
pub mod hooks;
pub mod host;
pub mod load;
pub mod manifest;
#[cfg(feature = "testkit")]
pub mod testkit;
pub mod tools;
pub mod worlds;
