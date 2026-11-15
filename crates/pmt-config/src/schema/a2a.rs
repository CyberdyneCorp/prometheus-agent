//! The `[a2a]` and `[peers]` sections (spec: agent-interop). `[a2a.server]`
//! is what this profile offers on the daemon's A2A listener (the listener
//! itself is machine-wide and lives in `daemon.toml [a2a]`);
//! `[a2a.agents.<name>]` are the A2A agents the outbound `a2a_*` tools
//! call; `[peers.<name>]` are other Prometheus gateways reached over their
//! API server. Every credential is a secret name, never a value.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::SecretName;

/// The shortest reply wait an A2A task may be configured with.
pub const MIN_REPLY_TIMEOUT_SECS: u64 = 300;
/// The most exchanges a context may be capped at.
pub const MAX_PINGPONG_TURNS: u32 = 20;
/// Agents one `a2a_orchestrate` call fans out to at most.
pub const MAX_ORCHESTRATE_AGENTS: usize = 8;

/// The `[a2a]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct A2aConfig {
    /// The inbound A2A server's view of this profile.
    pub server: A2aServerConfig,
    /// A2A agents the `a2a_*` tools call, keyed by the name the model uses.
    pub agents: BTreeMap<String, A2aAgent>,
}

/// `[a2a.server]`.
#[allow(clippy::struct_excessive_bools)] // independent switches
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct A2aServerConfig {
    /// Serve this profile on the daemon's A2A listener (`/p/<profile>/`,
    /// and `/` for `default`).
    #[schemars(extend("x-apply" = "restart"))]
    pub enabled: bool,
    /// The Agent Card's name (unset: `<hostname>-<profile>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// The Agent Card's description (unset: a generic one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The URL peers call (a reverse proxy's); unset: the bound address.
    /// Its host is also accepted in the `Host` header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// Build the card's URL from `X-Forwarded-Host`/`X-Forwarded-Proto`
    /// (only behind a proxy that sets them).
    pub trust_forwarded_headers: bool,
    /// Toolsets the card advertises as skills (unset: every toolset
    /// inbound A2A sessions get); a toolset they cannot use is never
    /// listed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advertised_toolsets: Option<Vec<String>>,
    /// Toolsets of inbound A2A sessions.
    pub toolsets: Vec<String>,
    /// Seconds a task waits for the agent's reply.
    #[schemars(range(min = 300, max = 86_400))]
    pub reply_timeout_secs: u64,
    /// Per-peer bearer tokens; the name drives trust, limits and audit.
    pub peers: Vec<A2aPeerToken>,
    /// A token shared by every caller, whose identity is the caller's IP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_token_secret: Option<SecretName>,
    /// Identities admitted to start turns.
    pub trusted_peers: Vec<String>,
    /// Admit every authenticated identity (WARN on every start).
    pub allow_all_peers: bool,
    /// Requests per minute per identity.
    #[schemars(range(min = 1, max = 100_000))]
    pub rate_limit_per_minute: u32,
    /// Exchanges per context within 10 minutes.
    #[schemars(range(min = 1, max = 20))]
    pub max_pingpong_turns: u32,
    /// Also record message text in the audit log.
    pub audit_content: bool,
    /// The secret signing push notifications (unset: the authenticating
    /// peer's token).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_secret: Option<SecretName>,
}

impl Default for A2aServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            agent_name: None,
            description: None,
            public_url: None,
            trust_forwarded_headers: false,
            advertised_toolsets: None,
            toolsets: vec!["api_server".to_owned()],
            reply_timeout_secs: MIN_REPLY_TIMEOUT_SECS,
            peers: Vec::new(),
            shared_token_secret: None,
            trusted_peers: Vec::new(),
            allow_all_peers: false,
            rate_limit_per_minute: 60,
            max_pingpong_turns: 5,
            audit_content: false,
            push_secret: None,
        }
    }
}

impl A2aServerConfig {
    /// Whether any bearer token is configured.
    #[must_use]
    pub fn has_tokens(&self) -> bool {
        !self.peers.is_empty() || self.shared_token_secret.is_some()
    }
}

/// One `[[a2a.server.peers]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct A2aPeerToken {
    /// The peer's identity.
    pub name: String,
    /// The secret holding its bearer token.
    pub token_secret: SecretName,
}

/// One `[a2a.agents.<name>]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct A2aAgent {
    /// The agent's base URL (where its Agent Card is served).
    pub url: String,
    /// The secret holding the bearer token it expects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_secret: Option<SecretName>,
    /// Seconds one call waits for the reply.
    #[serde(default = "default_agent_timeout")]
    #[schemars(range(min = 1, max = 86_400))]
    pub timeout_secs: u64,
    /// Capabilities `a2a_orchestrate` matches.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

fn default_agent_timeout() -> u64 {
    120
}

/// One `[peers.<name>]` entry: another gateway's API server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PeerGateway {
    /// The peer's API server base URL.
    pub url: String,
    /// A note shown by `gateway peer list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The secret holding the peer's API server key.
    pub key_secret: SecretName,
}

/// Whether `name` is a valid peer name (`^[a-z][a-z0-9-]{0,31}$`).
#[must_use]
pub fn valid_peer_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The secret a peer's key is stored under (`PEER_<NAME>_KEY`).
#[must_use]
pub fn peer_key_secret(name: &str) -> String {
    format!("PEER_{}_KEY", name.to_ascii_uppercase().replace('-', "_"))
}

#[cfg(test)]
mod tests {
    use super::{peer_key_secret, valid_peer_name};

    #[test]
    fn peer_names_follow_the_pattern() {
        for good in ["spark", "home-lab", "a1", &"a".repeat(32)] {
            assert!(valid_peer_name(good), "{good}");
        }
        for bad in ["", "Spark", "1lab", "lab_x", &"a".repeat(33), "-x"] {
            assert!(!valid_peer_name(bad), "{bad}");
        }
        assert_eq!(peer_key_secret("home-lab"), "PEER_HOME_LAB_KEY");
    }
}
