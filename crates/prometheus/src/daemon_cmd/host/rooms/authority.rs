//! Rooms across gateways (spec: bot-mode, Hosted rooms across gateways;
//! Room authority transfer): replicas copied from the authority's log,
//! promotion and demotion (never merging diverged histories: the old
//! writer must be fenced first), room grants a member gateway issues to an
//! authority, and the member side of an authority run.

use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{
    ReplicaState, RoomAuthority, RoomDemoteParams, RoomDone, RoomGrant, RoomGrantParams,
    RoomMemberGrantParams, RoomParams, RoomPromoteParams, RoomReplicateParams, RoomRevokeParams,
};
use pmt_store::Store;
use pmt_store::rooms::{EntryRow, MemberRow, RoomRow};
use serde_json::{Value, json};

use super::{api_error, log_page, params, to_value};
use crate::daemon_cmd::host::DaemonHost;
use crate::daemon_cmd::peers::client::Peer;

/// The secret a remote member's grant is kept under at the authority.
pub(super) fn grant_secret(room: &str, position: usize) -> String {
    format!("ROOM_GRANT_{}_{position}", room.to_uppercase())
}

fn state_of(store: &Store, room: &RoomRow) -> Result<ReplicaState, Error> {
    Ok(ReplicaState {
        room_id: room.id.clone(),
        last_seq: store.room_last_seq(&room.id)?,
        latest_seq: room.latest_seq,
        role: room.role.clone(),
        authority_gateway_id: room.authority_gateway.clone(),
        authority_epoch: room.authority_epoch,
    })
}

impl DaemonHost {
    /// The authority, replica and grant methods.
    pub(super) async fn authority_call(
        &self,
        store: &Arc<Store>,
        profile: &str,
        method: &str,
        p: Value,
    ) -> Result<Value, Error> {
        match method {
            "groups.replicate" => Box::pin(self.replicate(store, profile, params(p)?)).await,
            "groups.replica_state" => {
                let room = store.room(&params::<RoomParams>(p)?.room_id)?;
                to_value(&state_of(store, &room)?)
            }
            "groups.promote" => promote(store, &params(p)?),
            "groups.demote" => demote(store, &params(p)?),
            "groups.grant" => {
                let p: RoomGrantParams = params(p)?;
                self.profile(&p.member)?;
                let token = store.room_grant(&p.room_id, &p.authority, &p.member, &p.scopes)?;
                to_value(&RoomGrant { token })
            }
            "groups.revoke" => {
                let p: RoomRevokeParams = params(p)?;
                store.room_revoke(&p.room_id, &p.authority)?;
                to_value(&RoomDone {})
            }
            "groups.member_grant" => self.keep_grant(store, &params(p)?),
            _ => Err(Error::new(
                &codes::RPC_NOT_SERVED,
                format!("{method} is not served"),
            )),
        }
    }

    /// Copies the authority's log pages into this gateway's replica.
    async fn replicate(
        &self,
        store: &Store,
        profile: &str,
        p: RoomReplicateParams,
    ) -> Result<Value, Error> {
        let client = Peer::of(&self.fresh_scope(profile)?, &p.from)?;
        loop {
            let after = store.room_last_seq(&p.room_id).unwrap_or(0);
            let body = json!({"after_seq": after, "limit": 200});
            let path = format!("/api/rooms/{}/log", p.room_id);
            let page = client
                .call(reqwest::Method::POST, None, &path, Some(&body))
                .await
                .map_err(|e| e.into_error(&p.from))?;
            let (room, entries) = replica_of(&page, &p.from)?;
            let done = entries.is_empty();
            store.room_replicate(&room, &entries)?;
            if done {
                break;
            }
        }
        to_value(&state_of(store, &store.room(&p.room_id)?)?)
    }

    /// Keeps the grant a remote member's gateway issued.
    fn keep_grant(&self, store: &Store, p: &RoomMemberGrantParams) -> Result<Value, Error> {
        let room = store.room(&p.room_id)?;
        let position = room
            .members
            .iter()
            .position(|m| m.member == p.member && m.gateway.is_some())
            .ok_or_else(|| super::invalid(format!("{} is not a remote member", p.member)))?;
        let launch = self.fresh_scope(&self.launch_profile)?;
        let secret = pmt_core::secret::SecretString::new(p.token.clone());
        launch
            .secrets()
            .store()
            .set(&grant_secret(&room.id, position), &secret)?;
        to_value(&RoomDone {})
    }

    /// The member side of `/api/rooms/<id>/...`: the log pages a replica
    /// reads, an authority run of a member of this home, and its state.
    pub(in crate::daemon_cmd::host) fn room_api(
        &self,
        _profile: &str,
        call: &pmt_api::BotsCall,
    ) -> Result<(u16, Value), Error> {
        let store = self.rooms_store()?;
        let path: Vec<&str> = call.path.iter().map(String::as_str).collect();
        match (call.method.as_str(), path.as_slice()) {
            ("POST", ["rooms", id, "log"]) => match log_page(&store, id, &call.body) {
                Ok(page) => Ok((200, page)),
                Err(err) => Ok((404, api_error(err.message()))),
            },
            ("POST", ["rooms", id, "turns"]) => self.member_run(&store, id, call),
            ("GET", ["rooms", _, "turns", run]) => Ok(match store.room_run_state(run)? {
                Some((state, reply, reason)) => (
                    200,
                    json!({"state": state, "reply": reply, "reason": reason}),
                ),
                None => (404, api_error("no such run")),
            }),
            _ => Ok((404, api_error("no such room route"))),
        }
    }

    /// Starts an authority's run of a member of this home in its own room
    /// session, under a room grant with the `post` scope.
    fn member_run(
        &self,
        store: &Store,
        id: &str,
        call: &pmt_api::BotsCall,
    ) -> Result<(u16, Value), Error> {
        let granted = match call.grant.as_deref() {
            Some(token) => store.room_grant_of(id, token)?,
            None => None,
        };
        let member = call.body["member"].as_str().unwrap_or_default();
        let Some((granted_member, scopes)) = granted else {
            return Ok((403, api_error("no room grant for this authority")));
        };
        if granted_member != member || !scopes.iter().any(|s| s == "post") {
            return Ok((403, api_error("the room grant does not cover this member")));
        }
        let run = call.body["run_id"].as_str().unwrap_or_default().to_owned();
        if store.room_run_state(&run)?.is_some() {
            return Ok((202, json!({"run_id": run})));
        }
        let room = RoomRow {
            id: id.to_owned(),
            name: call.body["room_name"].as_str().unwrap_or(id).to_owned(),
            ..room_stub()
        };
        let text = call.body["text"].as_str().unwrap_or_default().to_owned();
        store.room_run(&run, (id, member), ("running", None, None))?;
        let (Some(host), Ok(profile)) = (self.arc(), self.profile(member)) else {
            return Ok((404, api_error("no such member")));
        };
        let store = self.rooms_store()?;
        pmt_core::spawn::spawn(async move {
            let ended = Box::pin(super::drive::local_turn(&host, &profile, &room, &text)).await;
            let (state, reply, reason) = match &ended {
                Ok(reply) => ("done", Some(reply.as_str()), None),
                Err(failure) => ("failed", None, Some(failure.as_str())),
            };
            if let Err(err) = store.room_run(
                &run,
                (&room.id, &profile.scope().name().to_string()),
                (state, reply, reason),
            ) {
                tracing::warn!(run, "room run not recorded: {}", err.message());
            }
        });
        Ok((202, json!({"run_id": call.body["run_id"]})))
    }
}

fn room_stub() -> RoomRow {
    RoomRow {
        id: String::new(),
        name: String::new(),
        picture: None,
        detect_stop_directives: true,
        role: "replica".to_owned(),
        authority_gateway: String::new(),
        authority_epoch: 1,
        source_peer: None,
        latest_seq: 0,
        needs_you: false,
        members: Vec::new(),
    }
}

/// The replica row and entries of one log page of peer `peer`.
fn replica_of(page: &Value, peer: &str) -> Result<(RoomRow, Vec<EntryRow>), Error> {
    let room = &page["room"];
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let members = room["members"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| MemberRow {
            member: text(&m["member"]),
            profile: text(&m["profile"]),
            gateway: m["gateway"].as_str().map(str::to_owned),
            held: m["held"].as_bool().unwrap_or(false),
            seen_seq: 0,
            state: "ok".to_owned(),
            reason: None,
            pending_run: None,
        })
        .collect();
    let row = RoomRow {
        id: text(&room["room_id"]),
        name: text(&room["name"]),
        picture: room["picture"].as_str().map(str::to_owned),
        detect_stop_directives: room["detect_stop_directives"].as_bool().unwrap_or(true),
        authority_gateway: text(&room["authority_gateway_id"]),
        authority_epoch: room["authority_epoch"].as_u64().unwrap_or(1),
        source_peer: Some(peer.to_owned()),
        latest_seq: page["latest_seq"].as_u64().unwrap_or(0),
        members,
        ..room_stub()
    };
    let entries = page["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| EntryRow {
            seq: e["seq"].as_u64().unwrap_or(0),
            kind: text(&e["kind"]),
            author: e["author"].as_str().map(str::to_owned),
            text: e["text"].as_str().map(str::to_owned),
            data: (!e["data"].is_null()).then(|| e["data"].to_string()),
            at: e["at"]
                .as_str()
                .and_then(|t| t.parse::<jiff::Timestamp>().ok())
                .map_or(0, jiff::Timestamp::as_millisecond),
        })
        .collect();
    if row.id.is_empty() {
        return Err(Error::new(&codes::PEER_REJECTED, "the peer sent no room"));
    }
    Ok((row, entries))
}

/// `groups.promote`: this gateway becomes the authority, one epoch past
/// the replicated lineage.
fn promote(store: &Store, p: &RoomPromoteParams) -> Result<Value, Error> {
    if !p.confirm {
        return Err(Error::new(
            &codes::ROOM_CONFIRMATION_REQUIRED,
            "confirmation_required: promoting a gateway needs confirm: true (fence the old authority first)",
        ));
    }
    let room = store.room(&p.room_id)?;
    let last = store.room_last_seq(&room.id)?;
    if last < room.latest_seq && !p.force_incomplete {
        return Err(Error::new(
            &codes::ROOM_REPLICA_INCOMPLETE,
            format!(
                "replica_incomplete: this replica has entries up to {last} of {}",
                room.latest_seq
            ),
        ));
    }
    let me = store.gateway_id()?;
    let epoch = room.authority_epoch + 1;
    store.room_authority(&room.id, "authority", (&me, epoch))?;
    let data = json!({"reason": p.reason, "authority_epoch": epoch}).to_string();
    store.room_append(
        &room.id,
        "authority.promoted",
        Some(&me),
        (None, Some(&data)),
    )?;
    to_value(&RoomAuthority {
        authority_gateway_id: me,
        authority_epoch: epoch,
    })
}

/// `groups.demote`: the old authority adopts the newer lineage (once) and
/// refuses further writes.
fn demote(store: &Store, p: &RoomDemoteParams) -> Result<Value, Error> {
    let room = store.room(&p.room_id)?;
    let already = room.role == "demoted"
        && room.authority_gateway == p.observed_gateway_id
        && room.authority_epoch == p.observed_epoch;
    if !already {
        store.room_authority(
            &room.id,
            "demoted",
            (&p.observed_gateway_id, p.observed_epoch),
        )?;
        let data = json!({"authority_gateway_id": p.observed_gateway_id, "authority_epoch": p.observed_epoch})
            .to_string();
        store.room_append(&room.id, "authority.lost", None, (None, Some(&data)))?;
    }
    to_value(&state_of(store, &store.room(&room.id)?)?)
}

#[cfg(test)]
mod tests {
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};

    use super::*;

    fn entry(seq: u64) -> EntryRow {
        EntryRow {
            seq,
            kind: "message".to_owned(),
            author: Some("ann".to_owned()),
            text: Some(format!("entry {seq}")),
            data: None,
            at: 0,
        }
    }

    /// A replica promoted while missing entries is refused with
    /// `replica_incomplete` (and stays a replica) unless
    /// `force_incomplete` is set.
    #[test]
    fn an_incomplete_replica_is_promoted_only_when_forced() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let store = Store::open(&scope).unwrap();
        let room = RoomRow {
            id: "room_1".to_owned(),
            name: "Ops".to_owned(),
            authority_gateway: "gw_old".to_owned(),
            latest_seq: 5,
            ..room_stub()
        };
        store.room_replicate(&room, &[entry(1), entry(2)]).unwrap();
        let mut params = RoomPromoteParams {
            profile: None,
            room_id: room.id.clone(),
            confirm: true,
            reason: None,
            force_incomplete: false,
        };
        let refused = promote(&store, &params).unwrap_err();
        assert_eq!(
            refused.code().code,
            codes::ROOM_REPLICA_INCOMPLETE.code,
            "{refused:?}"
        );
        assert!(refused.message().contains("up to 2 of 5"), "{refused:?}");
        assert_eq!(store.room(&room.id).unwrap().role, "replica");
        params.force_incomplete = true;
        let promoted = promote(&store, &params).unwrap();
        assert_eq!(promoted["authority_epoch"], 2);
        assert_eq!(store.room(&room.id).unwrap().role, "authority");
    }
}
