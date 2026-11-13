//! The desktop's profile-scoped calls (spec: desktop-app, Settings write
//! validated config patches; Profile switching is explicit; Bot chats are
//! created atomically by the daemon). Each names its profile: a desktop
//! window never relies on the connection's default.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Method;

/// One change of a `config.patch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigChange {
    /// A dotted key (`delegation.model`).
    pub key: String,
    /// The new value (typed by the key's schema); `null` or absent removes
    /// the key so its default applies.
    #[serde(default)]
    pub value: Option<Value>,
}

/// `config.patch` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatchParams {
    /// The window's profile.
    pub profile: String,
    /// Applied together, validated as a whole: all or none.
    pub changes: Vec<ConfigChange>,
}

/// `config.patch` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatched {
    /// The keys the patch named.
    pub keys: Vec<String>,
}

/// A call that only names the window's profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileParams {
    /// The window's profile.
    pub profile: String,
}

/// `config.reload` result: the profile's configuration state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigReloaded {
    /// The profile.
    pub profile: String,
    /// Valid edits loaded since the daemon started.
    pub reloads: u64,
    /// Why the file on disk is not in use, while it is invalid.
    #[serde(default)]
    pub error: Option<String>,
    /// Keys that apply after a daemon restart.
    #[serde(default)]
    pub pending_restart: Vec<String>,
}

/// `bot.chat.ensure` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotChat {
    /// The profile's canonical Bot Chat.
    pub session_id: String,
    /// Whether this call created it.
    pub created: bool,
}

method!(/// `config.patch`: validates the whole patch against the config
    /// schema and applies it atomically to the profile's `config.toml`; a
    /// violation leaves the file unchanged.
    ConfigPatch = "config.patch" (ConfigPatchParams) -> ConfigPatched);
method!(/// `config.reload`: re-reads the profile's `config.toml` now.
    ConfigReload = "config.reload" (ProfileParams) -> ConfigReloaded);
method!(/// `bot.chat.ensure`: the profile's canonical Bot Chat, created in
    /// one get-or-create transaction when it has none.
    BotChatEnsure = "bot.chat.ensure" (ProfileParams) -> BotChat);
