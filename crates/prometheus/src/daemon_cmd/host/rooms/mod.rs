//! Group chats in the daemon (spec: bot-mode, Group chats; Room stop
//! directives and held members; Durable room driver; Hosted rooms across
//! gateways; Room authority transfer): the `groups.*` methods and the
//! member and replication side of `/api/rooms/...`. The rooms live in the
//! default profile's store; [`drive`] runs their discussions.

mod authority;
mod drive;

use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{
    Room, RoomCapabilities, RoomCreateParams, RoomDeleteParams, RoomDone, RoomEntry, RoomLog,
    RoomLogParams, RoomMember, RoomMemberParams, RoomParams, RoomPostParams, RoomPosted,
    RoomUpdateParams, Rooms,
};
use pmt_store::Store;
use pmt_store::rooms::{EntryRow, MemberChange, MemberRow, RoomRow};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::DaemonHost;
use crate::bot_host::rooms::addressed;

/// The smallest and largest room.
const MEMBERS: std::ops::RangeInclusive<usize> = 2..=6;

fn params<T: DeserializeOwned>(value: Value) -> Result<T, Error> {
    serde_json::from_value(value)
        .map_err(|e| Error::new(&codes::RPC_INVALID_PARAMS, format!("invalid params: {e}")))
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, Error> {
    serde_json::to_value(value).map_err(|e| Error::new(&codes::RPC_INTERNAL, e.to_string()))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(&codes::RPC_INVALID_PARAMS, message)
}

/// The refusal of a write on a gateway that is not the room's authority.
pub(super) fn not_authority(room: &RoomRow) -> Error {
    Error::new(
        &codes::ROOM_NOT_AUTHORITY,
        format!("room authority is {}", room.authority_gateway),
    )
}

/// A log entry on the wire.
fn entry(e: &EntryRow) -> RoomEntry {
    RoomEntry {
        seq: e.seq,
        kind: e.kind.clone(),
        author: e.author.clone(),
        text: e.text.clone(),
        data: e.data.as_deref().and_then(|d| serde_json::from_str(d).ok()),
        at: jiff::Timestamp::from_millisecond(e.at)
            .map(|t| t.to_string())
            .unwrap_or_default(),
    }
}

/// A room on the wire.
fn room_of(row: &RoomRow) -> Room {
    Room {
        room_id: row.id.clone(),
        name: row.name.clone(),
        picture: row.picture.clone(),
        detect_stop_directives: row.detect_stop_directives,
        members: row
            .members
            .iter()
            .map(|m| RoomMember {
                member: m.member.clone(),
                profile: m.profile.clone(),
                gateway: m.gateway.clone(),
                held: m.held,
                state: m.state.clone(),
                reason: m.reason.clone(),
            })
            .collect(),
        role: row.role.clone(),
        authority_gateway_id: row.authority_gateway.clone(),
        authority_epoch: row.authority_epoch,
        latest_seq: row.latest_seq,
        needs_you: row.needs_you,
        running: drive::running(&row.id),
    }
}

impl DaemonHost {
    /// The store the home's rooms live in: the default profile's.
    pub(super) fn rooms_store(&self) -> Result<Arc<Store>, Error> {
        if let Ok(served) = self.profile("default") {
            return Ok(Arc::clone(&served.store));
        }
        let launch = self.profile(&self.launch_profile)?.scope();
        let store = crate::bot_host::send::open_store(&launch, "default")?;
        Ok(Arc::new(store))
    }

    /// `profile`'s configuration as its files say now (a peer registered
    /// a moment ago counts before the next reload).
    pub(super) fn fresh_scope(&self, profile: &str) -> Result<pmt_config::ProfileScope, Error> {
        let served = self.profile(profile)?.scope();
        let home = pmt_config::Home::open(served.home())?;
        pmt_config::ProfileScope::open(
            home.existing_profile(served.name())?,
            &pmt_config::Overrides::default(),
            pmt_config::Launch::Other,
        )
    }

    /// One `groups.*` call made for `profile`.
    pub(super) async fn room_call(
        &self,
        profile: &str,
        method: &str,
        p: Value,
    ) -> Result<Value, Error> {
        let store = self.rooms_store()?;
        match method {
            "groups.create" => self.room_create(&store, profile, params(p)?),
            "groups.list" => to_value(&Rooms {
                rooms: store.rooms()?.iter().map(room_of).collect(),
            }),
            "groups.get" => to_value(&room_of(&store.room(&params::<RoomParams>(p)?.room_id)?)),
            "groups.update" => room_update(&store, &params(p)?),
            "groups.delete" => room_delete(&store, &params(p)?),
            "groups.post" => room_post(&store, &params(p)?),
            "groups.stop" => {
                self.room_stop(&store, &params::<RoomParams>(p)?.room_id)
                    .await
            }
            "groups.compress_member" => Box::pin(self.compress_member(&store, params(p)?)).await,
            "groups.capabilities" => {
                self.room_capabilities(&store, &params::<RoomParams>(p)?.room_id)
            }
            "groups.log" => room_log(&store, &params(p)?),
            method => Box::pin(self.authority_call(&store, profile, method, p)).await,
        }
    }

    fn room_create(
        &self,
        store: &Store,
        asking: &str,
        p: RoomCreateParams,
    ) -> Result<Value, Error> {
        if !MEMBERS.contains(&p.members.len()) {
            return Err(invalid("a room has 2 to 6 member bots"));
        }
        let peers: Vec<String> = self
            .fresh_scope(asking)?
            .config()
            .peers
            .keys()
            .cloned()
            .collect();
        let mut members = Vec::new();
        for name in &p.members {
            let name = name.trim_start_matches('@');
            let (gateway, profile) = match name.split_once('/') {
                Some((peer, agent)) if peers.iter().any(|p| p == peer) => {
                    (Some(peer.to_owned()), agent.to_owned())
                }
                _ => {
                    let served = self.profile(name)?;
                    if !served.scope().config().bot_mode.enabled {
                        return Err(invalid(format!("{name} has Bot Mode turned off")));
                    }
                    (None, name.to_owned())
                }
            };
            members.push(MemberRow {
                member: name.to_owned(),
                profile,
                gateway,
                held: false,
                seen_seq: 0,
                state: "ok".to_owned(),
                reason: None,
                pending_run: None,
            });
        }
        let id = format!(
            "r{}",
            &ulid::Ulid::generate().to_string().to_lowercase()[10..]
        );
        let row = RoomRow {
            id: id.clone(),
            name: p.name.trim().to_owned(),
            picture: p.picture,
            detect_stop_directives: p.detect_stop_directives.unwrap_or(true),
            role: "authority".to_owned(),
            authority_gateway: store.gateway_id()?,
            authority_epoch: 1,
            source_peer: None,
            latest_seq: 0,
            needs_you: false,
            members,
        };
        store.room_create(&row)?;
        to_value(&room_of(&store.room(&id)?))
    }

    /// Cancels the running member turn and the queued continuations.
    async fn room_stop(&self, store: &Store, id: &str) -> Result<Value, Error> {
        let room = store.room(id)?;
        if room.role != "authority" {
            return Err(not_authority(&room));
        }
        if drive::cancel(id) {
            for member in room.members.iter().filter(|m| m.gateway.is_none()) {
                self.stop_member_turn(id, member).await;
            }
        }
        to_value(&RoomDone {})
    }

    /// Stops a local member's running room turn.
    async fn stop_member_turn(&self, room: &str, member: &MemberRow) {
        let Ok(profile) = self.profile(&member.profile) else {
            return;
        };
        let Ok(Some(session)) = drive::member_session_id(&profile.store, room) else {
            return;
        };
        let Ok(id) = super::session_id(&session) else {
            return;
        };
        if let Some(loaded) = profile.loaded.lock().await.get(&id).cloned() {
            loaded.stop();
        }
    }

    async fn compress_member(&self, store: &Store, p: RoomMemberParams) -> Result<Value, Error> {
        let room = store.room(&p.room_id)?;
        let member = room
            .members
            .iter()
            .find(|m| m.member == p.member && m.gateway.is_none())
            .ok_or_else(|| invalid(format!("{} is not a member of this home", p.member)))?;
        let profile = self.profile(&member.profile)?;
        let session = drive::member_session(&profile.store, &room)?;
        Box::pin(self.compress_session(&profile, &session)).await?;
        to_value(&RoomDone {})
    }

    fn room_capabilities(&self, store: &Store, id: &str) -> Result<Value, Error> {
        let room = store.room(id)?;
        let config = self
            .profile(&self.launch_profile)?
            .scope()
            .config()
            .hosted_rooms
            .clone();
        to_value(&RoomCapabilities {
            driver: room.role == "authority",
            hosted: room.members.iter().any(|m| m.gateway.is_some()),
            max_rounds: config.discussion.max_rounds,
            max_messages: config.discussion.max_messages,
        })
    }

    /// The rooms `profile` is a member of.
    pub(super) fn groups_of(&self, profile: &str) -> Vec<String> {
        self.rooms_store()
            .and_then(|store| store.rooms())
            .unwrap_or_default()
            .into_iter()
            .filter(|r| {
                r.members
                    .iter()
                    .any(|m| m.gateway.is_none() && m.profile == profile)
            })
            .map(|r| r.id)
            .collect()
    }

    /// Drives the discussions this daemon holds the lease of.
    pub(super) fn drive_rooms(&self) {
        let (Ok(store), Some(host)) = (self.rooms_store(), self.arc()) else {
            return;
        };
        for room in store.rooms().unwrap_or_default() {
            if room.role == "authority" && drive::pending(&store, &room.id) {
                drive::start(&host, &store, &room.id);
            }
        }
    }
}

fn room_update(store: &Store, p: &RoomUpdateParams) -> Result<Value, Error> {
    let room = store.room(&p.room_id)?;
    if room.role != "authority" {
        return Err(not_authority(&room));
    }
    let name = p.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    store.room_update(
        &p.room_id,
        name,
        p.picture.as_deref(),
        p.detect_stop_directives,
    )?;
    to_value(&room_of(&store.room(&p.room_id)?))
}

fn room_delete(store: &Store, p: &RoomDeleteParams) -> Result<Value, Error> {
    if !p.confirm {
        return Err(invalid(
            "deleting a room deletes its log: pass confirm: true",
        ));
    }
    store.room(&p.room_id)?;
    store.room_delete(&p.room_id)?;
    to_value(&RoomDone {})
}

/// A user message: holds and releases members by its directives and
/// mentions, then the driver runs the discussion.
fn room_post(store: &Store, p: &RoomPostParams) -> Result<Value, Error> {
    let room = store.room(&p.room_id)?;
    if room.role != "authority" {
        return Err(not_authority(&room));
    }
    let names: Vec<String> = room.members.iter().map(|m| m.member.clone()).collect();
    let addressed = addressed(&p.text, &names, room.detect_stop_directives);
    let seq = store.room_append(&room.id, "message", Some("user"), (Some(&p.text), None))?;
    store.room_needs_you(&room.id, false)?;
    for member in &room.members {
        let hold = addressed.held.contains(&member.member);
        let release =
            member.held && (addressed.everyone || addressed.members.contains(&member.member));
        if hold || release {
            let change = MemberChange {
                held: Some(hold),
                ..MemberChange::default()
            };
            store.room_member(&room.id, &member.member, &change)?;
            let kind = if hold { "held" } else { "released" };
            store.room_append(&room.id, kind, Some(&member.member), (None, None))?;
        }
    }
    to_value(&RoomPosted {
        seq,
        held: addressed.held,
    })
}

/// A page of the log, with the member error summary.
fn room_log(store: &Store, p: &RoomLogParams) -> Result<Value, Error> {
    let room = store.room(&p.room_id)?;
    let entries = store.room_log(&room.id, p.after_seq, p.limit.unwrap_or(200).min(1000))?;
    let activity = store
        .room_log(&room.id, 0, u32::MAX)?
        .iter()
        .filter(|e| e.kind == "turn.settled")
        .filter_map(|e| {
            let data: Value = serde_json::from_str(e.data.as_deref()?).ok()?;
            (data["outcome"] == "failed").then(|| {
                format!(
                    "{} hit an error — {}",
                    e.author.as_deref().unwrap_or("a member"),
                    data["error"].as_str().unwrap_or("unknown")
                )
            })
        })
        .collect();
    to_value(&RoomLog {
        latest_seq: store.room_last_seq(&room.id)?,
        room: room_of(&room),
        entries: entries.iter().map(entry).collect(),
        activity,
    })
}

/// The JSON a `/api/rooms/<id>/log` page answers (replication).
pub(super) fn log_page(store: &Store, id: &str, body: &Value) -> Result<Value, Error> {
    let after = body["after_seq"].as_u64().unwrap_or(0);
    let limit = body["limit"]
        .as_u64()
        .map_or(200, |n| u32::try_from(n).unwrap_or(200));
    room_log(
        store,
        &RoomLogParams {
            profile: None,
            room_id: id.to_owned(),
            after_seq: after,
            limit: Some(limit),
        },
    )
}

/// `{"error": {"message": ...}}`.
pub(super) fn api_error(message: &str) -> Value {
    json!({"error": {"message": message}})
}
