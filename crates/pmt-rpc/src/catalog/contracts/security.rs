//! Contracts of the security boundaries (spec: security-boundaries,
//! Per-session network egress policy; Vault management CLI).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// `egress.set` params: changes applied in order (mode, then allow, then
/// deny); an empty change only reads the policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressSetParams {
    /// The session.
    pub session_id: String,
    /// `public`, `allowlist` or `none`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Host patterns to admit (`host`, `host:port`, `*.suffix`).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Host patterns to refuse in every mode.
    #[serde(default)]
    pub deny: Vec<String>,
}

/// `egress.set` result: the session's policy after the change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EgressState {
    /// The mode.
    pub mode: String,
    /// Admitted patterns.
    pub allow: Vec<String>,
    /// Refused patterns.
    pub deny: Vec<String>,
}

method!(/// `egress.set`: narrows or extends a session's egress policy; the
    /// system prompt and tool array stay as they are.
    EgressSet = "egress.set" (EgressSetParams) -> EgressState);

/// One vault item's metadata (never a secret value; spec:
/// security-boundaries, Vault management CLI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultItem {
    /// `login_...`, `card_...` or `address_...`.
    pub handle: String,
    /// `login`, `card` or `address`.
    pub kind: String,
    /// The label.
    pub label: String,
    /// A login's origins.
    pub origins: Vec<String>,
    /// A login's identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// Whether a login has a TOTP seed.
    pub totp: bool,
}

/// `vault.list` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultListParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
}

/// `vault.list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultItems {
    /// Every item's metadata.
    pub items: Vec<VaultItem>,
}

/// A login to add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultLoginInput {
    /// Origins it fills on (`https://host`).
    pub origins: Vec<String>,
    /// User name or email.
    pub identifier: String,
    /// The password.
    pub password: String,
    /// A base32 TOTP seed.
    #[serde(default)]
    pub totp: Option<String>,
}

/// A payment card to add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultCardInput {
    /// Cardholder name.
    pub holder: String,
    /// Card number.
    pub number: String,
    /// `MM/YY`.
    pub expiry: String,
    /// Security code.
    pub cvc: String,
}

/// An address to add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultAddressInput {
    /// Full name.
    pub name: String,
    /// Street line.
    pub line1: String,
    /// Second line.
    #[serde(default)]
    pub line2: String,
    /// City.
    pub city: String,
    /// State or region.
    #[serde(default)]
    pub region: String,
    /// Postal code.
    pub postal_code: String,
    /// Country.
    pub country: String,
}

/// `vault.add` params: exactly one of `login`, `card`, `address`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultAddParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// A name for the item.
    pub label: String,
    /// A login.
    #[serde(default)]
    pub login: Option<VaultLoginInput>,
    /// A payment card.
    #[serde(default)]
    pub card: Option<VaultCardInput>,
    /// An address.
    #[serde(default)]
    pub address: Option<VaultAddressInput>,
}

/// `vault.remove` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultRemoveParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// The item's handle.
    pub handle: String,
}

/// `vault.sources` params: reads, or enables or disables one source.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultSourcesParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// `onepassword` or `bitwarden` to enable.
    #[serde(default)]
    pub enable: Option<String>,
    /// `onepassword` or `bitwarden` to disable.
    #[serde(default)]
    pub disable: Option<String>,
}

/// One password-manager source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultSource {
    /// `onepassword` or `bitwarden`.
    pub name: String,
    /// `vault.<name>.enabled`.
    pub enabled: bool,
    /// Whether this build can read it.
    pub available: bool,
}

/// `vault.sources` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultSourceList {
    /// The sources after the change.
    pub sources: Vec<VaultSource>,
}

method!(/// `vault.list`: the profile's vault items, metadata only.
    VaultList = "vault.list" (VaultListParams) -> VaultItems);
method!(/// `vault.add`: adds a login, card or address; answers its metadata.
    VaultAdd = "vault.add" (VaultAddParams) -> VaultItem);
method!(/// `vault.remove`: deletes an item; answers its metadata.
    VaultRemove = "vault.remove" (VaultRemoveParams) -> VaultItem);
method!(/// `vault.sources`: password-manager login sources.
    VaultSources = "vault.sources" (VaultSourcesParams) -> VaultSourceList);
