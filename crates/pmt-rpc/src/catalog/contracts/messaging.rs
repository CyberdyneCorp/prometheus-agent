//! Contracts of the messaging gateway (spec: messaging-gateway, Documented
//! pairing flow).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Event, Method};

/// `pairing.approve` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PairingApproveParams {
    /// The profile; required when the connection is granted several.
    #[serde(default)]
    pub profile: Option<String>,
    /// The platform the code was issued on.
    pub platform: String,
    /// The code (spaces, dashes and case are ignored).
    pub code: String,
}

/// `pairing.approve` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PairingApproved {
    /// The platform.
    pub platform: String,
    /// The approved sender's platform id.
    pub sender_id: String,
    /// The sender's display name, when known.
    pub sender_name: Option<String>,
}

method!(/// `pairing.approve`: approves a pending pairing code of the
    /// profile; the sender is told in their DM.
    PairingApprove = "pairing.approve" (PairingApproveParams) -> PairingApproved);

/// `adapter.state` payload: one messaging adapter changed state (spec:
/// platform-adapters, Adapter state reporting and reconnect).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdapterStatePayload {
    /// The owning profile.
    pub profile: String,
    /// Platform name.
    pub platform: String,
    /// `disabled`, `connecting`, `connected`, `degraded`, `retrying` or
    /// `fatal`.
    pub state: String,
    /// Error code of a `degraded`, `retrying` or `fatal` adapter.
    #[serde(default)]
    pub code: Option<String>,
    /// What happened.
    #[serde(default)]
    pub message: Option<String>,
}

event!(/// `adapter.state`: a messaging adapter of a profile the client is
    /// attached to changed state.
    AdapterStateChanged = "adapter.state" (AdapterStatePayload));
