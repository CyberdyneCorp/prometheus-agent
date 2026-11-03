//! The `[plugins]` section (spec: plugin-system): opt-in enablement, trusted
//! project plugin directories, hook and load bounds, WASM resource limits,
//! safe mode, the catalog and package signature policy, and each plugin's
//! own settings.

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
    /// Bound on instantiating and registering one plugin; a plugin that
    /// overruns is skipped with `load timed out` while the others load.
    #[schemars(range(min = 1, max = 600_000))]
    pub load_timeout_ms: u64,
    /// Skip every non-embedded plugin, MCP server and python-bridge
    /// sidecar (as `prometheus --safe-mode` does, without its other effects).
    pub safe_mode: bool,
    /// Where the signed live plugin catalog is fetched from (cached 6 hours
    /// under the profile's `cache/`); empty uses only the catalog embedded
    /// in the binary.
    pub catalog_url: String,
    /// What installing an unsigned local `.pmtpkg` needs: `require` (only
    /// with `--allow-unsigned`), `warn` or `off`. Catalog installs always
    /// need a valid signature unless `--allow-unsigned` is given.
    pub signature_policy: SignaturePolicy,
    /// Statically scan every plugin install and update for threats
    /// (reverse shells, credential exfiltration, destructive commands,
    /// persistence, obfuscated execution, prompt injection); `false` skips
    /// the scan and the install record says so.
    pub scan_on_install: bool,
    /// The plugin approval transport that presents approval requests,
    /// `<plugin_id>:<name>`; empty asks on the session's own surface.
    pub approval_transport: String,
    /// What a failing, timed-out, unbound or missing approval transport
    /// leads to: `deny` the request, or ask on the session's own surface
    /// (`builtin`).
    pub approval_transport_fallback: TransportFallback,
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
            load_timeout_ms: 10_000,
            safe_mode: false,
            catalog_url: DEFAULT_CATALOG_URL.to_owned(),
            signature_policy: SignaturePolicy::Require,
            scan_on_install: true,
            approval_transport: String::new(),
            approval_transport_fallback: TransportFallback::Deny,
            wasm: PluginsWasmConfig::default(),
            entries: BTreeMap::new(),
        }
    }
}

/// The live catalog of the project's repository; its signature is
/// `<url>.minisig`.
pub const DEFAULT_CATALOG_URL: &str = "https://raw.githubusercontent.com/CyberdyneCorp/prometheus-agent/main/crates/pmt-plugins/catalog/catalog.json";

/// `plugins.approval_transport_fallback`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransportFallback {
    /// Deny the request.
    Deny,
    /// Ask on the session's own surface.
    Builtin,
}

/// `plugins.signature_policy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SignaturePolicy {
    /// An unsigned local package installs only with `--allow-unsigned`.
    Require,
    /// An unsigned local package installs with a warning.
    Warn,
    /// Local packages are not checked for a signature.
    Off,
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
