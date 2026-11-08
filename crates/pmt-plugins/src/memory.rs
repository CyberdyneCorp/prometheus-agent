//! Memory providers hosted outside the WASM runtime (spec: python-bridge,
//! Scope of hosted code; memory, External memory provider contract): a
//! Hermes `MemoryProvider` runs in its plugin's bridge sidecar, and the
//! host reaches it through [`MemoryCalls`] with the same calls a
//! `memory-provider` WASM plugin answers.

use std::sync::Arc;
use std::time::Duration;

use pmt_core::error::Error;
use serde_json::Value;

/// Calls into one out-of-process memory provider: `method` is a
/// `memory-provider` world function in snake case (`prefetch`,
/// `sync_turn`...), `args` its named arguments.
pub trait MemoryCalls: Send + Sync {
    /// One call, bounded by `timeout` (the plugin's call timeout when
    /// `None`): the provider's answer, its own error as `Ok(Err(..))`, a
    /// host failure (crash, timeout) as `Err`.
    ///
    /// # Errors
    ///
    /// The sidecar could not answer.
    fn call(
        &self,
        method: &str,
        args: &Value,
        timeout: Option<Duration>,
    ) -> Result<Result<Value, String>, Error>;
}

/// A loaded plugin's out-of-process memory provider.
#[derive(Clone)]
pub struct ExternalMemory {
    /// The plugin id (`memory.provider`).
    pub plugin: String,
    /// Its calls.
    pub calls: Arc<dyn MemoryCalls>,
}

impl std::fmt::Debug for ExternalMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalMemory")
            .field("plugin", &self.plugin)
            .finish_non_exhaustive()
    }
}
