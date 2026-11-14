//! The desktop UI toolset's daemon methods (spec: desktop-app, Terminal
//! pane tools; First-run onboarding chat): `process.list` and
//! `process.log` feed the read-only terminal tabs that mirror a session's
//! background processes (any attached client may read them), and
//! `onboarding.ensure` opens the profile's onboarding chat.

use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::{Need, attached_profile, profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// Registers the methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::ProcessListCall, _, _>(catalog, list)?
        .register::<c::ProcessLogCall, _, _>(catalog, log)?
        .register::<c::OnboardingEnsure, _, _>(catalog, onboarding)?;
    Ok(())
}

async fn list(call: Call, params: c::SessionRef) -> Result<c::ProcessList, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    call.server
        .host()
        .processes(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))
}

async fn log(call: Call, params: c::ProcessRef) -> Result<c::ProcessLog, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    call.server
        .host()
        .process_log(&profile, params)
        .await
        .map_err(|e| rpc(&e))
}

async fn onboarding(call: Call, params: c::ProfileParams) -> Result<c::OnboardingChat, RpcError> {
    let profile = profile_for(&call, Some(&params.profile))?;
    let (session_id, created) = call
        .server
        .host()
        .onboarding_chat(&profile)
        .await
        .map_err(|e| rpc(&e))?;
    Ok(c::OnboardingChat {
        session_id,
        created,
    })
}
