//! The connector gateway's JSON shapes (camelCase on the wire). Unknown
//! fields are ignored so a newer gateway stays readable.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `POST v1/connectors/search`.
pub(crate) const SEARCH: &str = "v1/connectors/search";
/// `POST v1/connectors/schemas`.
pub(crate) const SCHEMAS: &str = "v1/connectors/schemas";
/// `POST v1/connectors/execute`.
pub(crate) const EXECUTE: &str = "v1/connectors/execute";
/// `POST v1/connectors/connections`.
pub(crate) const CONNECTIONS: &str = "v1/connectors/connections";
/// `GET v1/connectors`.
pub(crate) const LIST: &str = "v1/connectors";
/// `GET v1/connectors/accounts`, `PATCH|DELETE .../<id>`.
pub(crate) const ACCOUNTS: &str = "v1/connectors/accounts";

/// One remote tool's schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSchema {
    /// Connector slug.
    pub connector: String,
    /// Vendor tool slug.
    pub tool: String,
    /// Description.
    #[serde(default)]
    pub description: String,
    /// JSON schema of the arguments.
    #[serde(default)]
    pub input_schema: Value,
}

/// The search answer: schemas of every hit, keyed by vendor slug.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchResponse {
    #[serde(default)]
    pub(crate) schemas: Map<String, Value>,
}

/// The schemas answer.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SchemasResponse {
    #[serde(default)]
    pub(crate) schemas: Map<String, Value>,
}

/// A successful execute.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecuteResponse {
    pub(crate) result: ExecuteResult,
}

/// One call's result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecuteResult {
    #[serde(default)]
    pub(crate) data: Value,
    /// The alias of the account the call ran under (absent: unnamed).
    #[serde(default)]
    pub(crate) account: Option<String>,
}

/// A gateway error body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayFailure {
    /// `CONNECTION_REQUIRED`, `TOOL_NOT_FOUND`, `ALIAS_REQUIRED`...
    #[serde(default)]
    pub code: String,
    /// Human text.
    #[serde(default)]
    pub message: String,
    /// The connector the error is about.
    #[serde(default)]
    pub connector: Option<String>,
    /// A link that would fix it (never forwarded to the model by calls).
    #[serde(default)]
    pub connect_url: Option<String>,
}

/// `POST v1/connectors/connections`: start (or repair) an authorization.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectRequest {
    /// The connectors.
    pub connectors: Vec<String>,
    /// Repair an existing account instead of adding one.
    pub reinitiate: bool,
    /// The alias of the account to create or repair.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// The existing account to repair.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
}

/// One connection start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectResult {
    /// Connector slug.
    pub connector: String,
    /// `active` (nothing to do), `initiated` (open `connect_url`) or
    /// `failed`.
    pub status: String,
    /// The authorization link for the user.
    #[serde(default)]
    pub connect_url: Option<String>,
    /// The account being created or repaired.
    #[serde(default)]
    pub connection_id: Option<String>,
    /// Its alias.
    #[serde(default)]
    pub alias: Option<String>,
    /// Why it failed.
    #[serde(default)]
    pub status_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ConnectResponse {
    #[serde(default)]
    pub(crate) results: Vec<ConnectResult>,
}

/// One connector of the account's catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorItem {
    /// Connector slug.
    pub connector: String,
    /// Whether an active account exists.
    #[serde(default)]
    pub connected: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ListResponse {
    #[serde(default)]
    pub(crate) items: Vec<ConnectorItem>,
}

/// One connector account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// The gateway's id.
    pub connection_id: String,
    /// Connector slug.
    pub connector: String,
    /// `pending`, `active`, `failed`, `expired`, `revoked` or `inactive`.
    pub status: String,
    /// The app's own label (an e-mail address...).
    #[serde(default)]
    pub label: String,
    /// The user's name for it.
    #[serde(default)]
    pub alias: Option<String>,
    /// Retired by a reconnect that replaced it: kept, never used.
    #[serde(default)]
    pub disabled: bool,
}

impl Account {
    /// Active and not retired.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.status == "active" && !self.disabled
    }

    /// Its alias, else its label.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.label)
    }

    /// Whether `name` is its alias or (when it has none) its label.
    #[must_use]
    pub fn answers_to(&self, name: &str) -> bool {
        self.alias.as_deref() == Some(name) || (self.alias.is_none() && self.label == name)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct AccountsResponse {
    #[serde(default)]
    pub(crate) accounts: Vec<Account>,
}
