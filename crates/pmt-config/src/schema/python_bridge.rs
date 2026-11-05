//! The `[python_bridge]` section (spec: python-bridge): the managed
//! interpreter, requirement pinning and the bounds of every sidecar.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The interpreter versions the bridge can run plugins on.
pub const PYTHON_VERSIONS: &[&str] = &["3.11", "3.12", "3.13"];

/// The `[python_bridge]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct PythonBridgeConfig {
    /// The managed CPython every bridged plugin, skill environment and
    /// named environment runs on (`3.11`, `3.12` or `3.13`).
    pub python_version: PythonVersion,
    /// Accept requirements without an upper bound (`requests>=2`); by
    /// default they are refused so an install never picks up a new major.
    pub allow_unbounded_requirements: bool,
    /// Stop a sidecar that has no hooks or providers after this many idle
    /// seconds (`0` keeps it running).
    pub idle_shutdown_secs: u64,
    /// Bound on one call into a sidecar (hook calls use
    /// `plugins.hook_timeout_ms`).
    #[schemars(range(min = 1, max = 3_600_000))]
    pub call_timeout_ms: u64,
    /// Resident memory of one sidecar's process tree; above it the sidecar
    /// is terminated as a crash.
    #[schemars(range(min = 16_777_216))]
    pub max_memory_bytes: u64,
    /// Processes in one sidecar's tree; above it the sidecar is terminated
    /// as a crash.
    #[schemars(range(min = 1, max = 4096))]
    pub max_processes: u32,
}

impl Default for PythonBridgeConfig {
    fn default() -> Self {
        Self {
            python_version: PythonVersion::V3_12,
            allow_unbounded_requirements: false,
            idle_shutdown_secs: 600,
            call_timeout_ms: 30_000,
            max_memory_bytes: 1_073_741_824,
            max_processes: 16,
        }
    }
}

/// A supported interpreter version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum PythonVersion {
    /// CPython 3.11.
    #[serde(rename = "3.11")]
    V3_11,
    /// CPython 3.12.
    #[serde(rename = "3.12")]
    V3_12,
    /// CPython 3.13.
    #[serde(rename = "3.13")]
    V3_13,
}

impl PythonVersion {
    /// `3.12`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V3_11 => "3.11",
            Self::V3_12 => "3.12",
            Self::V3_13 => "3.13",
        }
    }

    /// Parses `3.12`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "3.11" => Some(Self::V3_11),
            "3.12" => Some(Self::V3_12),
            "3.13" => Some(Self::V3_13),
            _ => None,
        }
    }
}

impl std::fmt::Display for PythonVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
