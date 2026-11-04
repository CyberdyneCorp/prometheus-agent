//! The `[connectors]` section (spec: connectors, Connector availability;
//! Connection flows require the user).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The connector gateway of the Nous tool gateway.
pub const DEFAULT_CONNECTOR_GATEWAY: &str = "https://connector-gateway.nousresearch.com";

/// The `[connectors]` section: hosted app integrations (Gmail, Notion,
/// Linear...) whose tools run on the Nous connector gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct ConnectorsConfig {
    /// Offer hosted connectors when the profile has a Nous sign-in.
    pub enabled: bool,
    /// How long a connection card or `connectors connect` waits for the
    /// user to finish signing in.
    #[schemars(range(min = 1, max = 3_600))]
    pub connect_timeout_secs: u64,
    /// The connector gateway's origin (a self-hosted gateway, or a test
    /// server); `https` except on loopback.
    pub gateway_url: String,
}

impl Default for ConnectorsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            connect_timeout_secs: 300,
            gateway_url: DEFAULT_CONNECTOR_GATEWAY.to_owned(),
        }
    }
}
