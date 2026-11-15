//! The desktop's profile-scoped methods (spec: desktop-app, Settings write
//! validated config patches; Profile switching is explicit; Bot chats are
//! created atomically by the daemon): `config.patch`, `config.reload` and
//! `bot.chat.ensure`; and the union roster and desktop relay (spec:
//! agent-interop): `bot.roster.list`, `bot.relay.drain`,
//! `bot.relay.deliver`. Each names its profile, which the connection's
//! ticket must grant.

use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::{profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// Registers the desktop's methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::ConfigPatch, _, _>(catalog, config_patch)?
        .register::<c::ConfigReload, _, _>(catalog, config_reload)?
        .register::<c::BotChatEnsure, _, _>(catalog, bot_chat_ensure)?
        .register::<c::BotRosterList, _, _>(catalog, bot_roster)?
        .register::<c::BotRelayDrain, _, _>(catalog, relay_drain)?
        .register::<c::BotRelayDeliver, _, _>(catalog, relay_deliver)?;
    Ok(())
}

async fn config_patch(
    call: Call,
    params: c::ConfigPatchParams,
) -> Result<c::ConfigPatched, RpcError> {
    let profile = profile_for(&call, Some(&params.profile))?;
    let keys = params.changes.iter().map(|c| c.key.clone()).collect();
    call.server
        .host()
        .config_patch(&profile, params.changes)
        .await
        .map_err(|e| rpc(&e))?;
    Ok(c::ConfigPatched { keys })
}

async fn config_reload(
    call: Call,
    params: c::ProfileParams,
) -> Result<c::ConfigReloaded, RpcError> {
    let profile = profile_for(&call, Some(&params.profile))?;
    call.server
        .host()
        .config_reload(&profile)
        .await
        .map_err(|e| rpc(&e))
}

async fn bot_chat_ensure(call: Call, params: c::ProfileParams) -> Result<c::BotChat, RpcError> {
    let profile = profile_for(&call, Some(&params.profile))?;
    let (session_id, created) = call
        .server
        .host()
        .bot_chat(&profile)
        .await
        .map_err(|e| rpc(&e))?;
    Ok(c::BotChat {
        session_id,
        created,
    })
}

async fn bot_roster(call: Call, params: c::RosterParams) -> Result<c::Roster, RpcError> {
    profile_for(&call, Some(&params.profile))?;
    call.server
        .host()
        .bot_roster(params)
        .await
        .map_err(|e| rpc(&e))
}

async fn relay_drain(call: Call, params: c::RelayDrainParams) -> Result<c::RelayDrained, RpcError> {
    profile_for(&call, Some(&params.profile))?;
    call.server
        .host()
        .relay_drain(params)
        .await
        .map_err(|e| rpc(&e))
}

async fn relay_deliver(
    call: Call,
    params: c::RelayDeliverParams,
) -> Result<c::RelayDelivered, RpcError> {
    profile_for(&call, Some(&params.profile))?;
    call.server
        .host()
        .relay_deliver(params)
        .await
        .map_err(|e| rpc(&e))
}
