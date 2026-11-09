//! The seam between the `llamacpp` provider and the local-models runtime
//! (spec: local-models, Managed server lifecycle; External llama-server
//! detection; Local runtime errors). A `llamacpp` route without a
//! `base_url` asks the [`LocalRuntime`] of its profile's credential extras
//! for an endpoint; the host (the `prometheus` binary) supplies one that
//! detects, reuses or starts a llama-server. Without one, or when it finds
//! no server, the route fails with `provider.local_runtime_down` and the
//! request is never sent anywhere else.

use pmt_core::error::{Error, codes};

use crate::resolve::BoxFuture;

/// The provider served by the local-models runtime.
pub const PROVIDER_ID: &str = "llamacpp";

/// Finds (or starts) the llama-server a `llamacpp` route without a
/// `base_url` uses.
pub trait LocalRuntime: Send + Sync {
    /// The server's OpenAI-compatible base URL.
    ///
    /// # Errors
    ///
    /// `provider.local_runtime_down` (69) naming the remedy.
    fn endpoint(&self) -> BoxFuture<'_, Result<String, Error>>;
}

/// The failure of a `llamacpp` route in a process with no local runtime.
#[must_use]
pub fn unavailable() -> Error {
    Error::new(
        &codes::PROVIDER_LOCAL_RUNTIME_DOWN,
        "the local model server isn't running (no local runtime in this process)",
    )
    .with_hint("set providers.llamacpp.base_url, or run 'prometheus local start'")
}
