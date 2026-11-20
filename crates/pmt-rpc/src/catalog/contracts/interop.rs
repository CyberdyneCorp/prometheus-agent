//! Agents across gateways (spec: agent-interop, Union roster across
//! gateways; Desktop relay for cross-connection bot messages): the union
//! roster (`bot.roster.list`), and the courier methods a desktop holding
//! several connections uses to carry bot messages between daemons that
//! never receive each other's credentials (`bot.relay.drain`,
//! `bot.relay.deliver`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Method;

/// `bot.roster.list` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RosterParams {
    /// The window's profile (its peers are the ones listed).
    pub profile: String,
    /// Also list registered peers' agents and the agents attached desktops
    /// reported for their other connections.
    #[serde(default)]
    pub include_remote: bool,
    /// Ask every peer again now instead of using rows younger than 300 s.
    #[serde(default)]
    pub refresh: bool,
    /// Also list hidden bots.
    #[serde(default)]
    pub include_hidden: bool,
}

/// An agent as a source reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RosterAgent {
    /// The profile name on its machine.
    pub name: String,
    /// The bot's title.
    #[serde(default)]
    pub title: Option<String>,
    /// What it does.
    #[serde(default)]
    pub description: Option<String>,
    /// Its avatar (a `BotAvatar`).
    #[serde(default)]
    pub avatar: Option<super::BotAvatar>,
    /// Its model.
    #[serde(default)]
    pub model: Option<String>,
    /// Whether it accepts bot messages.
    #[serde(default = "yes")]
    pub accepts_bot_messages: bool,
}

fn yes() -> bool {
    true
}

/// One roster row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RosterRow {
    /// `@<name>`, or `@<name>-<device>` when several sources serve the
    /// name.
    pub handle: String,
    /// The agent.
    pub agent: RosterAgent,
    /// `local`, `peer` or `connection`.
    pub source: String,
    /// `local`, the peer's name or the connection's label.
    pub device: String,
    /// The source could not be reached: this is its last-known row.
    pub stale: bool,
    /// Why the source could not be reached.
    #[serde(default)]
    pub error: Option<String>,
    /// A local bot's Bot Chat and presentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<RosterChat>,
}

/// A local bot's Bot Chat as the roster shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RosterChat {
    /// The latest message of its Bot Chat, shortened.
    pub preview: Option<String>,
    /// When that message was written (RFC 3339).
    pub last_message_at: Option<String>,
    /// Messages since the Bot Chat was last read.
    pub unread: u64,
    /// It owns a running turn, wrote within the last 90 seconds, or has a
    /// worker heartbeat.
    pub active: bool,
    /// Hidden from the default roster.
    pub hidden: bool,
    /// Its section.
    pub section: Option<super::BotSection>,
}

/// One source of the roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RosterSource {
    /// `local`, `peer` or `connection`.
    pub source: String,
    /// `local`, the peer's name or the connection's label.
    pub device: String,
    /// Whether its last listing succeeded.
    pub ok: bool,
    /// Why not.
    #[serde(default)]
    pub error: Option<String>,
    /// When it was last listed (RFC 3339).
    #[serde(default)]
    pub listed_at: Option<String>,
}

/// `bot.roster.list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Roster {
    /// Every agent.
    pub rows: Vec<RosterRow>,
    /// Every source asked.
    pub sources: Vec<RosterSource>,
}

/// One connection an attached desktop holds, with its agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelayConnection {
    /// The connection's label.
    pub label: String,
    /// Its agents.
    pub agents: Vec<RosterAgent>,
}

/// `bot.relay.drain` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelayDrainParams {
    /// The profile whose outbox is drained.
    pub profile: String,
    /// The desktop's other connections and their agents (the roster lists
    /// them as `connection` rows).
    #[serde(default)]
    pub connections: Vec<RelayConnection>,
}

/// A bot message bound for an agent on another connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelayEnvelope {
    /// Never runs twice on the target.
    pub delivery_id: String,
    /// The sender's handle.
    pub sender: String,
    /// `<handle>@<connection>`.
    pub target: String,
    /// The message.
    pub message: String,
    /// The sending daemon's signature over the fields above.
    pub signature: String,
    /// When it was queued (RFC 3339).
    pub created_at: String,
}

/// `bot.relay.drain` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelayDrained {
    /// Envelopes to deliver.
    pub envelopes: Vec<RelayEnvelope>,
    /// Seconds the courier has to deliver them before they are offered
    /// again.
    pub deadline_secs: u64,
}

/// The target's answer to an envelope, carried back to the sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelayReply {
    /// The envelope's delivery id.
    pub delivery_id: String,
    /// The envelope's signature (proves the courier holds the envelope).
    pub signature: String,
    /// The reply, when the turn completed.
    #[serde(default)]
    pub reply: Option<String>,
    /// The failure reason otherwise.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `bot.relay.deliver` params: an envelope for its target, or a reply for
/// its sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelayDeliverParams {
    /// The profile that runs the envelope, or that sent the replied one.
    pub profile: String,
    /// An envelope to run as the profile's Bot Chat's next turn.
    #[serde(default)]
    pub envelope: Option<RelayEnvelope>,
    /// A reply to settle in the profile's outbox.
    #[serde(default)]
    pub reply: Option<RelayReply>,
}

/// `bot.relay.deliver` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelayDelivered {
    /// The delivery id.
    pub delivery_id: String,
    /// `delivered`, `failed`, `running` (a retried envelope whose turn
    /// still runs), `settled` (a reply recorded) or `ignored` (a reply
    /// nobody waits for).
    pub outcome: String,
    /// The target's reply.
    #[serde(default)]
    pub reply: Option<String>,
    /// The failure reason.
    #[serde(default)]
    pub reason: Option<String>,
}

method!(/// `bot.roster.list`: the profiles of this daemon as bots, with the
    /// agents of registered peers and other desktop connections when
    /// `include_remote`.
    BotRosterList = "bot.roster.list" (RosterParams) -> Roster);
method!(/// `bot.relay.drain`: the envelopes of the profile's outbox a courier
    /// may deliver now.
    BotRelayDrain = "bot.relay.drain" (RelayDrainParams) -> RelayDrained);
method!(/// `bot.relay.deliver`: runs an envelope as the profile's Bot Chat's
    /// next turn (never twice per delivery id), or settles a reply.
    BotRelayDeliver = "bot.relay.deliver" (RelayDeliverParams) -> RelayDelivered);
