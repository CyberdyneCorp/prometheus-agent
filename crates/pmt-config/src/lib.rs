//! Typed `config.toml` schema, layering, validation, migrations, profiles
//! and the secrets store; the single source of configuration defaults.
//!
//! Holds the profile config root, the machine-wide `daemon.toml` schema
//! ([`daemon`]) and home resolution ([`home`]). The remaining sections,
//! layering and validation are expanded in M1 group 5.

pub mod daemon;
pub mod home;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use daemon::DaemonConfig;

/// The `config.toml` schema version this build writes and reads.
pub const SCHEMA_VERSION: u32 = 1;

// Unknown keys are rejected so a typo never silently falls back to a
// default. The M1 sections are expanded in M1 group 5 (task 5.3). Doc
// comments here become schema descriptions, so they stay user-facing.
/// Root of a profile's `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Prometheus config.toml")]
pub struct Config {
    /// Version of this file's schema; older files are migrated forward.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Outbound networking.
    #[serde(default)]
    pub network: NetworkConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            network: NetworkConfig::default(),
        }
    }
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

/// The `[network]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// Resolve and connect every outbound client (providers, MCP over HTTP,
    /// web tools, adapters, OTLP, downloads) over IPv4 only; loopback
    /// listeners are unaffected. Set it when `prometheus doctor network
    /// --online` reports a blackholed IPv6 route.
    #[serde(default)]
    pub force_ipv4: bool,
}
