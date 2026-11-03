//! The `[plugins]` section (spec: plugin-system): opt-in enablement, trusted
//! project plugin directories, hook bounds, WASM resource limits and each
//! plugin's own settings.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[plugins]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct PluginsConfig {
    /// Ids of the non-embedded plugins to instantiate; a discovered plugin
    /// stays disabled until listed here (`prometheus plugins enable <id>`).
    pub enabled: Vec<String>,
    /// Ids never instantiated; wins over `enabled`.
    pub disabled: Vec<String>,
    /// Project roots whose `.prometheus/plugins/` directory is scanned;
    /// any other project's plugins are never read.
    pub trusted_project_dirs: Vec<String>,
    /// Bound on one hook callback; a callback that overruns counts as no
    /// result and is suspended for 60 seconds.
    #[schemars(range(min = 1, max = 600_000))]
    pub hook_timeout_ms: u64,
    /// `pre_llm_call` context of one callback above this many characters
    /// is spilled to `hook_outputs/<session>/` and replaced by its head,
    /// tail and the spill path.
    #[schemars(range(min = 1))]
    pub hook_context_max_chars: u64,
    /// WASM plugin limits.
    pub wasm: PluginsWasmConfig,
    /// Each plugin's own settings, `plugins.entries.<id>.<key>`, read and
    /// written by the plugin through its `config` host interface.
    pub entries: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: Vec::new(),
            disabled: Vec::new(),
            trusted_project_dirs: Vec::new(),
            hook_timeout_ms: 30_000,
            hook_context_max_chars: 10_000,
            wasm: PluginsWasmConfig::default(),
            entries: BTreeMap::new(),
        }
    }
}

/// The `[plugins.wasm]` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct PluginsWasmConfig {
    /// Linear memory cap of one plugin instance.
    #[schemars(range(min = 65_536))]
    pub max_memory_bytes: u64,
    /// CPU bound of one call into a plugin (fuel and epoch interruption);
    /// an overrun fails the call with `plugin '<id>' exceeded call timeout`.
    #[schemars(range(min = 1, max = 600_000))]
    pub call_timeout_ms: u64,
}

impl Default for PluginsWasmConfig {
    fn default() -> Self {
        Self {
            max_memory_bytes: 268_435_456,
            call_timeout_ms: 30_000,
        }
    }
}
