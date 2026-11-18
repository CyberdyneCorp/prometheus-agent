//! `screen.takeover` and `screen.handback` (spec: execution-environments,
//! Bot screen takeover lease): a viewer takes the profile's bot screen
//! from the agent and hands it back.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// `screen.takeover` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenTakeoverParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// Who takes it over, as shown to the agent and other viewers (the
    /// client kind when omitted); the same name hands it back after a
    /// reconnect.
    #[serde(default)]
    pub viewer: Option<String>,
}

/// `screen.handback` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenHandbackParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
    /// The viewer handing it back (the client kind when omitted).
    #[serde(default)]
    pub viewer: Option<String>,
    /// Clear the lease whoever holds it.
    #[serde(default)]
    pub force: bool,
}

/// The lease after a takeover or hand-back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenLeaseState {
    /// Bumped by every takeover; results of agent calls started under an
    /// older epoch are void.
    pub epoch: u64,
    /// The viewer holding the screen; `None`: the agent has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
}

method!(/// `screen.takeover`: the viewer takes the bot screen over; the
    /// most recent takeover wins.
    ScreenTakeover = "screen.takeover" (ScreenTakeoverParams) -> ScreenLeaseState);
method!(/// `screen.handback`: the holder (or anyone with `force`) hands the
    /// bot screen back to the agent.
    ScreenHandback = "screen.handback" (ScreenHandbackParams) -> ScreenLeaseState);

/// `screen.ticket` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenTicketParams {
    /// The profile (the connection's only one when omitted).
    #[serde(default)]
    pub profile: Option<String>,
}

/// A single-use display ticket.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenTicket {
    /// The secret: offered as the `prometheus-ticket.<ticket>` WebSocket
    /// subprotocol next to `prometheus-display.v1`, on `path`.
    pub ticket: String,
    /// The display stream's path on the daemon's listener (`/screen`).
    pub path: String,
    /// Seconds until it expires (30).
    pub expires_in_secs: u64,
}

method!(/// `screen.ticket`: a single-use 30-second ticket for the bot
    /// screen's RFB stream.
    ScreenTicketCall = "screen.ticket" (ScreenTicketParams) -> ScreenTicket);
