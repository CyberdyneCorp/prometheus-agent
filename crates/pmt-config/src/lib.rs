//! Typed `config.toml` schema, layering, validation, migrations, profiles
//! and the secrets store; the single source of configuration defaults.
//!
//! M0 holds only the config root the generated JSON Schema is built from;
//! the sections, layering and validation are expanded in M1 group 5.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
        }
    }
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}
