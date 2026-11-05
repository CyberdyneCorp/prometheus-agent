//! What the plugin host knows about Python plugins without running Python
//! (spec: python-bridge): the Hermes API table the shim implements, the
//! `plugin.yaml` translation and the static compatibility audit. The
//! bridge itself (`pmt-pybridge`) runs them.

pub mod api;
pub mod audit;
pub mod hermes;
mod source;

pub use hermes::{HERMES_MANIFEST, PythonLaunch};
