//! Bot Mode's methods (spec: bot-mode): `bot.*` (metadata, creating,
//! duplicating and deleting bots, routines, sections) and `groups.*`
//! (group chats and room authority). Each call names the profile it acts
//! for, which the connection's grant must cover; the host runs it and
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
}

/// Registers the methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    add::<c::BotMetaGet>(router, catalog)?;
    add::<c::BotMetaUpdate>(router, catalog)?;
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
        let value = call
            .server
            .host()
            .bots(
                &profile,
                BotRequest {
                    method: M::NAME,
                    params,
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
