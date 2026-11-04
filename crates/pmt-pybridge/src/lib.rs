//! The python bridge (spec: python-bridge; design D7): Hermes-style Python
//! plugins run out of process, each in its own sandboxed sidecar on a
//! managed CPython, speaking length-prefixed JSON-RPC over stdio, behind a
//! compatibility shim of the Hermes `PluginContext` API. Prometheus never
//! imports Python into its own process.
//!
//! - [`runtime`]: the managed interpreter and uv.
//! - [`env`]: requirement pinning and per-plugin environments.
//! - [`wire`], [`sidecar`]: the protocol and one process.
//! - [`supervisor`]: lifecycle, restarts, stall detection, idle shutdown.
//! - [`bridged`]: a plugin's registrations as plugin-host contributions.
//! - [`shim`]: the Python package compiled into the binary.

pub mod bridged;
pub mod env;
pub mod host;
pub mod launch;
pub mod limits;
pub mod log;
pub mod provider;
pub mod registrations;
pub mod runtime;
pub mod shim;
pub mod sidecar;
pub mod skills;
pub mod supervisor;
#[cfg(feature = "testkit")]
pub mod testkit;
pub mod wire;

/// Lower-case hex of `bytes` (digests).
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}
