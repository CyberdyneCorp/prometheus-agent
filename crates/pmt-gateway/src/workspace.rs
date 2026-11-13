//! A session's workspace for the desktop's panes (spec: desktop-app, File
//! browser pane; Chat transcript rendering): `fs.list` and `fs.read`
//! read through the session's own terminal backend, for any attached
//! client (observers read too).

use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::{Need, attached_profile, rpc};
use crate::router::Router;
use crate::server::Call;

/// Registers the workspace methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::FsList, _, _>(catalog, list)?
        .register::<c::FsRead, _, _>(catalog, read)?
        .register::<c::ArtifactsListCall, _, _>(catalog, artifacts)?;
    Ok(())
}

async fn list(call: Call, params: c::FsListParams) -> Result<c::FsListing, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    let link = call.server.link(&params.session_id);
    call.server
        .host()
        .fs_list(&profile, params, link)
        .await
        .map_err(|e| rpc(&e))
}

async fn read(call: Call, params: c::FsReadParams) -> Result<c::FsFile, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    let link = call.server.link(&params.session_id);
    call.server
        .host()
        .fs_read(&profile, params, link)
        .await
        .map_err(|e| rpc(&e))
}

async fn artifacts(
    call: Call,
    params: c::ArtifactsListParams,
) -> Result<c::ArtifactList, RpcError> {
    let profile = crate::methods::profile_for(&call, params.profile.as_deref())?;
    if let Some(session_id) = &params.session_id {
        attached_profile(&call, session_id, Need::Attached)?;
    }
    call.server
        .host()
        .artifacts(&profile, params)
        .await
        .map_err(|e| rpc(&e))
}
