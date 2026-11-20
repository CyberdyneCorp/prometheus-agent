//! Bot Mode's methods (spec: bot-mode): `bot.*` (metadata, creating,
//! duplicating and deleting bots, routines, sections) and `groups.*`
//! (group chats and room authority). Each call names the profile it acts
//! for, which the connection's grant must cover, and so must every other
//! profile it acts on (a clone source, a room member, the member a room
//! grant is for); the host runs it, checking rooms against the grant, and
//! answers the method's result.

use pmt_rpc::Catalog;
use pmt_rpc::contracts::{self as c, Method};
use serde_json::Value;

use crate::methods::{profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// One Bot Mode call: the method name and its params.
#[derive(Debug, Clone, PartialEq)]
pub struct BotRequest {
    /// The method name.
    pub method: &'static str,
    /// Its params (as the contract parsed them).
    pub params: Value,
    /// The profiles the connection is granted: a room call acts on the
    /// room's local members, which the host checks against it.
    pub granted: Vec<String>,
}

/// The profiles other than `profile` a call names and acts on: the
/// source `bot.create` clones (config, memory and API keys), the local
/// members `groups.create` puts in a room (whose turns run with their keys
/// and tools) and the member `groups.grant` lets an authority drive.
/// Remote members (`peer/agent`) are their own gateway's to check.
fn acted_on(method: &str, params: &Value) -> Vec<String> {
    let text = |key: &str| {
        params[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match method {
        "bot.create" => text("clone_from").into_iter().collect(),
        "groups.grant" => text("member").into_iter().collect(),
        "groups.create" => params["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|m| m.trim_start_matches('@'))
            .filter(|m| !m.contains('/'))
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Registers the methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    add::<c::BotMetaGet>(router, catalog)?;
    add::<c::BotMetaUpdate>(router, catalog)?;
    add::<c::BotAvatarGet>(router, catalog)?;
    add::<c::BotCreate>(router, catalog)?;
    add::<c::BotDuplicate>(router, catalog)?;
    add::<c::BotDelete>(router, catalog)?;
    add::<c::BotRoutinesList>(router, catalog)?;
    add::<c::BotChatRead>(router, catalog)?;
    add::<c::BotSectionsDelete>(router, catalog)?;
    add::<c::VoiceSpeak>(router, catalog)?;
    for register in GROUPS {
        register(router, catalog)?;
    }
    Ok(())
}

type Register = fn(&mut Router, &Catalog) -> Result<(), String>;

/// The `groups.*` methods.
const GROUPS: &[Register] = &[
    add::<c::GroupsCreate>,
    add::<c::GroupsList>,
    add::<c::GroupsGet>,
    add::<c::GroupsUpdate>,
    add::<c::GroupsDelete>,
    add::<c::GroupsPost>,
    add::<c::GroupsStop>,
    add::<c::GroupsCompressMember>,
    add::<c::GroupsCapabilities>,
    add::<c::GroupsLog>,
    add::<c::GroupsReplicate>,
    add::<c::GroupsReplicaState>,
    add::<c::GroupsPromote>,
    add::<c::GroupsDemote>,
    add::<c::GroupsGrant>,
    add::<c::GroupsRevoke>,
    add::<c::GroupsMemberGrant>,
];

fn add<M: Method>(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router.register::<M, _, _>(catalog, |call: Call, params: M::Params| async move {
        let params = serde_json::to_value(&params).unwrap_or(Value::Null);
        let profile = profile_for(&call, params.get("profile").and_then(Value::as_str))?;
        for other in acted_on(M::NAME, &params) {
            profile_for(&call, Some(&other))?;
        }
        let value = call
            .server
            .host()
            .bots(
                &profile,
                BotRequest {
                    method: M::NAME,
                    params,
                    granted: call.conn.grant.profiles.clone(),
                },
            )
            .await
            .map_err(|e| rpc(&e))?;
        serde_json::from_value::<M::Result>(value).map_err(|e| {
            rpc(&pmt_core::error::Error::new(
                &pmt_core::error::codes::RPC_INTERNAL,
                format!(
                    "{}: the host's answer does not match the contract: {e}",
                    M::NAME
                ),
            ))
        })
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::acted_on;

    #[test]
    fn calls_name_every_local_profile_they_act_on() {
        assert_eq!(
            acted_on("bot.create", &json!({"profile": "a", "clone_from": "b"})),
            ["b"]
        );
        assert!(acted_on("bot.create", &json!({"profile": "a", "clone_from": null})).is_empty());
        assert_eq!(
            acted_on("groups.grant", &json!({"member": "b", "authority": "peer"})),
            ["b"]
        );
        assert_eq!(
            acted_on(
                "groups.create",
                &json!({"members": ["@a", "b", "laptop/c"]})
            ),
            ["a", "b"]
        );
        assert!(acted_on("groups.post", &json!({"room_id": "r1"})).is_empty());
    }
}
