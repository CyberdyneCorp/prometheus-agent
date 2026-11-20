//! Group chats (spec: bot-mode, Group chats; Room stop directives and held
//! members; Durable room driver; Hosted rooms across gateways; Room
//! authority transfer): rooms of 2 to 6 member bots driven by the room
//! authority, their log and its replicas, and room grants between
//! gateways.
//!
//! Promotion and demotion never merge diverged histories: fence the old
//! writer (stop its daemon or demote it) before promoting a replacement.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Method;

/// A room member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomMember {
    /// Its handle in the room (`<profile>`, or `<peer>/<agent>`).
    pub member: String,
    /// Its profile on its own machine.
    pub profile: String,
    /// The peer gateway it lives on (none: this home).
    #[serde(default)]
    pub gateway: Option<String>,
    /// Held by a stop directive: no turns until mentioned again.
    pub held: bool,
    /// `ok` or `unreachable`.
    pub state: String,
    /// Why it is unreachable (a failure reason code).
    #[serde(default)]
    pub reason: Option<String>,
}

/// A room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Room {
    /// Stable id.
    pub room_id: String,
    /// Unique display name.
    pub name: String,
    /// A picture (data URL or blob reference).
    #[serde(default)]
    pub picture: Option<String>,
    /// `stop`/`halt`/`pause` next to a mention holds that member.
    pub detect_stop_directives: bool,
    /// The members.
    pub members: Vec<RoomMember>,
    /// `authority`, `replica` or `demoted`.
    pub role: String,
    /// The gateway that owns the log.
    pub authority_gateway_id: String,
    /// The authority's lineage.
    pub authority_epoch: u64,
    /// The last log entry.
    pub latest_seq: u64,
    /// A member asked for the user (`@user`).
    pub needs_you: bool,
    /// A discussion is running.
    pub running: bool,
}

/// Params naming a room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
}

/// `groups.create` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomCreateParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// Unique display name.
    pub name: String,
    /// 2 to 6 members: profiles of this home, or `<peer>/<agent>`.
    pub members: Vec<String>,
    /// A picture.
    #[serde(default)]
    pub picture: Option<String>,
    /// Default true.
    #[serde(default)]
    pub detect_stop_directives: Option<bool>,
}

/// `groups.list` params.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RoomsParams {
    /// The profile asking (the window's).
    pub profile: Option<String>,
}

/// `groups.list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rooms {
    /// Every room.
    pub rooms: Vec<Room>,
}

/// `groups.update` params: rename (history and sessions stay), picture,
/// stop directive detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomUpdateParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
    /// A new unique name.
    #[serde(default)]
    pub name: Option<String>,
    /// A new picture.
    #[serde(default)]
    pub picture: Option<String>,
    /// Stop directive detection.
    #[serde(default)]
    pub detect_stop_directives: Option<bool>,
}

/// `groups.delete` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomDeleteParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
    /// Must be true.
    #[serde(default)]
    pub confirm: bool,
}

/// `groups.post` params: a user message to the room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomPostParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
    /// The message.
    pub text: String,
}

/// `groups.post` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomPosted {
    /// The message's log entry.
    pub seq: u64,
    /// The members a stop directive in it holds now.
    #[serde(default)]
    pub held: Vec<String>,
}

/// `groups.compress_member` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomMemberParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
    /// The member.
    pub member: String,
}

/// `groups.capabilities` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomCapabilities {
    /// This daemon drives the room's discussions itself (every member
    /// lives here, or it is the authority of a hosted room).
    pub driver: bool,
    /// Members on other gateways take part.
    pub hosted: bool,
    /// `hosted_rooms.discussion.max_rounds`.
    pub max_rounds: u32,
    /// `hosted_rooms.discussion.max_messages`.
    pub max_messages: u32,
}

/// One room log entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomEntry {
    /// Its sequence number.
    pub seq: u64,
    /// `message`, `turn.started`, `turn.settled`, `pass`, `held`,
    /// `released`, `settled`, `stopped`, `authority.lost`, ...
    pub kind: String,
    /// `user`, a member, or `room`.
    #[serde(default)]
    pub author: Option<String>,
    /// Its text.
    #[serde(default)]
    pub text: Option<String>,
    /// Details (outcome, failure reason, round).
    #[serde(default)]
    pub data: Option<Value>,
    /// When (RFC 3339).
    pub at: String,
}

/// `groups.log` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomLogParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
    /// Entries after this one.
    #[serde(default)]
    pub after_seq: u64,
    /// At most this many (default 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `groups.log` result: one page of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomLog {
    /// The room (with its members).
    pub room: Room,
    /// The page.
    pub entries: Vec<RoomEntry>,
    /// The last entry of the log.
    pub latest_seq: u64,
    /// The member error summary ("<member> hit an error — <first line>").
    #[serde(default)]
    pub activity: Vec<String>,
}

/// `groups.replicate` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomReplicateParams {
    /// The profile whose peer registry names the authority.
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
    /// The authority, a registered peer.
    pub from: String,
}

/// `groups.replica_state` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplicaState {
    /// The room.
    pub room_id: String,
    /// The last entry this gateway holds.
    pub last_seq: u64,
    /// The last entry the authority reported.
    pub latest_seq: u64,
    /// `authority`, `replica` or `demoted`.
    pub role: String,
    /// The authority.
    pub authority_gateway_id: String,
    /// Its lineage.
    pub authority_epoch: u64,
}

/// `groups.promote` params: makes this gateway the room authority.
/// Fence the old writer first (stop its daemon, or have it run
/// `groups.demote`): promotion never merges diverged histories, so an old
/// writer still accepting messages forks the room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomPromoteParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
    /// Must be true.
    #[serde(default)]
    pub confirm: bool,
    /// Why (kept in the log).
    #[serde(default)]
    pub reason: Option<String>,
    /// Promote a replica that is missing entries.
    #[serde(default)]
    pub force_incomplete: bool,
}

/// `groups.promote` result: the new authority and lineage. The old writer
/// must already be fenced; pass these to its `groups.demote`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomAuthority {
    /// The authority now.
    pub authority_gateway_id: String,
    /// Its lineage.
    pub authority_epoch: u64,
}

/// `groups.demote` params (all required): fences this gateway as an old
/// writer. It adopts the newer lineage and refuses further room writes;
/// fence the old writer like this (or stop its daemon) before promoting a
/// replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomDemoteParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
    /// The new authority.
    pub observed_gateway_id: String,
    /// Its lineage.
    pub observed_epoch: u64,
}

/// `groups.grant` params: lets a room authority drive a member of this
/// home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomGrantParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
    /// The authority's gateway id.
    pub authority: String,
    /// The member (a profile of this home).
    pub member: String,
    /// `post`, `stop`, `approve`.
    pub scopes: Vec<String>,
}

/// `groups.grant` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomGrant {
    /// The grant token (shown once; give it to the authority's owner).
    pub token: String,
}

/// `groups.revoke` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomRevokeParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
    /// The authority.
    pub authority: String,
}

/// `groups.member_grant` params: the authority keeps a member gateway's
/// grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomMemberGrantParams {
    /// The profile asking (the window's).
    #[serde(default)]
    pub profile: Option<String>,
    /// The room.
    pub room_id: String,
    /// The remote member (`<peer>/<agent>`).
    pub member: String,
    /// The token its gateway issued.
    pub token: String,
}

/// An empty result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomDone {}

method!(/// `groups.create`: a room of 2 to 6 member bots.
    GroupsCreate = "groups.create" (RoomCreateParams) -> Room);
method!(/// `groups.list`: every room of this home.
    GroupsList = "groups.list" (RoomsParams) -> Rooms);
method!(/// `groups.get`: one room.
    GroupsGet = "groups.get" (RoomParams) -> Room);
method!(/// `groups.update`: renames a room (a taken name is refused),
    /// changes its picture or stop directive detection.
    GroupsUpdate = "groups.update" (RoomUpdateParams) -> Room);
method!(/// `groups.delete`: deletes a room and its log.
    GroupsDelete = "groups.delete" (RoomDeleteParams) -> RoomDone);
method!(/// `groups.post`: a user message that drives a discussion (refused
    /// with "room authority is <gateway>" on a non-authority).
    GroupsPost = "groups.post" (RoomPostParams) -> RoomPosted);
method!(/// `groups.stop`: cancels the active member turn and queued
    /// continuations.
    GroupsStop = "groups.stop" (RoomParams) -> RoomDone);
method!(/// `groups.compress_member`: compresses one member's room session.
    GroupsCompressMember = "groups.compress_member" (RoomMemberParams) -> RoomDone);
method!(/// `groups.capabilities`: whether this daemon drives the room.
    GroupsCapabilities = "groups.capabilities" (RoomParams) -> RoomCapabilities);
method!(/// `groups.log`: a page of the room log.
    GroupsLog = "groups.log" (RoomLogParams) -> RoomLog);
method!(/// `groups.replicate`: copies the authority's log pages into this
    /// gateway's replica.
    GroupsReplicate = "groups.replicate" (RoomReplicateParams) -> ReplicaState);
method!(/// `groups.replica_state`: how far this gateway's copy is.
    GroupsReplicaState = "groups.replica_state" (RoomParams) -> ReplicaState);
method!(/// `groups.promote`: makes this gateway the room authority (the old
    /// writer must be fenced first; histories are never merged).
    GroupsPromote = "groups.promote" (RoomPromoteParams) -> RoomAuthority);
method!(/// `groups.demote`: the old authority adopts the newer lineage and
    /// refuses further room writes.
    GroupsDemote = "groups.demote" (RoomDemoteParams) -> ReplicaState);
method!(/// `groups.grant`: a revocable grant for a room authority to drive a
    /// member of this home.
    GroupsGrant = "groups.grant" (RoomGrantParams) -> RoomGrant);
method!(/// `groups.revoke`: revokes an authority's grants for a room.
    GroupsRevoke = "groups.revoke" (RoomRevokeParams) -> RoomDone);
method!(/// `groups.member_grant`: keeps the grant a remote member's gateway
    /// issued.
    GroupsMemberGrant = "groups.member_grant" (RoomMemberGrantParams) -> RoomDone);
