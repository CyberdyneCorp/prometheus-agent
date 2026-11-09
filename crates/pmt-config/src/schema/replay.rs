//! The `[replay]` section (spec: replay-and-evaluation, Content-addressed
//! model-call cache; Cache administration and limits).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[replay]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ReplayConfig {
    /// The content-addressed model-call cache.
    pub cache: ModelCallCacheConfig,
}

/// The `[replay.cache]` section: model responses stored by request hash
/// under `<profile>/cache/model-calls/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ModelCallCacheConfig {
    /// `off`, `read` (serve stored responses), `write` (store new ones) or
    /// `read_write`. Off for interactive sessions; the offline demo (and
    /// eval runs) use `read_write`.
    pub mode: CacheMode,
    /// Entries of different namespaces never serve each other.
    #[schemars(length(min = 1, max = 64), regex(pattern = r"^[A-Za-z0-9_.-]+$"))]
    pub namespace: String,
    /// Least-recently-used entries are evicted above this size.
    #[schemars(range(min = 1))]
    pub max_bytes: u64,
}

impl Default for ModelCallCacheConfig {
    fn default() -> Self {
        Self {
            mode: CacheMode::Off,
            namespace: DEFAULT_NAMESPACE.to_owned(),
            max_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

/// The default `replay.cache.namespace`.
pub const DEFAULT_NAMESPACE: &str = "default";

/// How model calls use the cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    /// Neither read nor written.
    #[default]
    Off,
    /// Stored responses are served; nothing new is stored.
    Read,
    /// Successful responses are stored; nothing is served.
    Write,
    /// Both.
    ReadWrite,
}

impl CacheMode {
    /// Whether stored responses are served.
    #[must_use]
    pub const fn reads(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    /// Whether new responses are stored.
    #[must_use]
    pub const fn writes(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }
}
