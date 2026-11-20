//! External secret sources (spec: security-boundaries, External secret
//! sources are opt-in): Bitwarden Secrets Manager and 1Password, both off
//! by default and never contacted while off.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::SecretName;

/// The default `cache_ttl_secs` of an external source.
pub const SOURCE_CACHE_TTL_SECS: u64 = 300;

/// `[secrets.bitwarden]`: every secret of a Bitwarden Secrets Manager
/// project, by its key (a bulk source), read with the `bws` CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BitwardenSourceConfig {
    /// Read secrets from Bitwarden (off by default: `bws` is then never
    /// run).
    pub enabled: bool,
    /// The stored secret holding the machine account's access token (the
    /// only place the token is read from).
    pub access_token_secret: SecretName,
    /// The project whose secrets are read (unset: every project the token
    /// sees).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// A self-hosted server's URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    /// The keys `prometheus secrets bitwarden sync --apply` accepted; the
    /// source supplies only these (empty: every key of the project).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// Resolved values stay in memory at most this long (never on disk).
    #[schemars(range(max = 86_400))]
    pub cache_ttl_secs: u64,
}

impl Default for BitwardenSourceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            access_token_secret: SecretName("BWS_ACCESS_TOKEN".to_owned()),
            project_id: None,
            server_url: None,
            names: Vec::new(),
            cache_ttl_secs: SOURCE_CACHE_TTL_SECS,
        }
    }
}

/// `[secrets.onepassword]`: secrets mapped by name to 1Password
/// references (a mapped source), read with the `op` CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OnePasswordSourceConfig {
    /// Read secrets from 1Password (off by default: `op` is then never
    /// run).
    pub enabled: bool,
    /// The stored secret holding the service-account token (the only
    /// place the token is read from).
    pub access_token_secret: SecretName,
    /// The vault `setup --vault` selected (what `sync` reads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault: Option<String>,
    /// Secret name to its `op://vault/item/field` reference.
    pub map: BTreeMap<String, String>,
    /// Resolved values stay in memory at most this long (never on disk).
    #[schemars(range(max = 86_400))]
    pub cache_ttl_secs: u64,
}

impl Default for OnePasswordSourceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            access_token_secret: SecretName("OP_SERVICE_ACCOUNT_TOKEN".to_owned()),
            vault: None,
            map: BTreeMap::new(),
            cache_ttl_secs: SOURCE_CACHE_TTL_SECS,
        }
    }
}
