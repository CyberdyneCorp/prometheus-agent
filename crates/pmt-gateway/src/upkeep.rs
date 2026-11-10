//! What an interactive client shows as a session opens or ends (spec:
//! memory, Reviewable memory changes per session; session-storage,
//! Low-disk prune prompt): `memory.summary` and `autoprune.offer`. The
//! client asks; the daemon cannot push either (see
//! [`pmt_rpc::contracts::MemorySummary`]). Also the profile's learning
//! journey for overlays (`journey.list`; spec: memory, Learning journey
//! timeline).

use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::{Need, attached_profile, profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// Registers `memory.summary`, `autoprune.offer` and `journey.list`.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::MemorySummaryCall, _, _>(catalog, memory_summary)?
        .register::<c::AutopruneOfferCall, _, _>(catalog, autoprune_offer)?
        .register::<c::JourneyListCall, _, _>(catalog, journey_list)?;
    Ok(())
}

async fn memory_summary(call: Call, params: c::SessionRef) -> Result<c::MemorySummary, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    let line = call
        .server
        .host()
        .memory_summary(&profile, &params.session_id)
        .map_err(|e| rpc(&e))?;
    Ok(c::MemorySummary { line })
}

async fn autoprune_offer(
    call: Call,
    params: c::AutopruneOfferParams,
) -> Result<c::AutopruneOffer, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    let question = call
        .server
        .host()
        .autoprune_offer(&profile)
        .map_err(|e| rpc(&e))?;
    Ok(c::AutopruneOffer { question })
}

async fn journey_list(
    call: Call,
    params: c::JourneyListParams,
) -> Result<c::JourneyList, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    call.server
        .host()
        .journey(&profile, params.since.as_deref())
        .map_err(|e| rpc(&e))
}
