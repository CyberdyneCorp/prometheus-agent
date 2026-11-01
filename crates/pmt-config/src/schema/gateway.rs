//! The `[gateway]` section: how a profile takes part in the home's gateway
//! daemon (spec: configuration-and-profiles, Served profile set).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::SecretName;

/// The `[gateway]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "restart"))]
pub struct GatewayConfig {
    /// In the daemon's launch profile: serve every profile of the home
    /// (each in its own scope) instead of the launch profile only.
    pub multiplex: bool,
    /// Never serve this profile from a multiplexing daemon (it gets a
    /// daemon of its own home).
    pub standalone: bool,
    /// Messaging platforms this profile answers on, keyed by platform
    /// (`telegram`, `discord`, ...). A profile clone leaves them out
    /// unless `--clone-channels` is given, so two profiles never answer
    /// the same bot account.
    #[schemars(extend("x-apply" = "next_session"))]
    pub platforms: BTreeMap<String, PlatformConfig>,
}

/// One `[gateway.platforms.<name>]` table: the keys every adapter shares.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformConfig {
    /// Start this adapter with the daemon.
    pub enabled: bool,
    /// The chat that receives notifications and scheduled output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_channel: Option<String>,
    /// Platform user ids admitted without pairing.
    pub allow_users: Vec<String>,
    /// Chats every member of which is admitted.
    pub allow_chats: Vec<String>,
    /// Admit everyone (logged at WARN on every start).
    pub allow_all: bool,
    /// Platform user ids with the admin command tier.
    pub admins: Vec<String>,
    /// The secret holding the adapter's credential (never the value).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_secret: Option<SecretName>,
}
