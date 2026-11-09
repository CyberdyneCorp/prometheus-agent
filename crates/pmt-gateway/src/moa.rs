//! `moa.presets.list`, `moa.presets.put` and `moa.presets.delete` (spec:
//! mixture-of-agents, Preset management commands): the dashboard and
//! desktop preset editors. Every method answers the preset list after
//! its change.

use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::{profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// A change to a profile's presets.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Create or replace preset `name`.
    Put {
        /// The preset name.
        name: String,
        /// The preset.
        preset: c::MoaPresetInput,
    },
    /// Delete preset `name`.
    Delete {
        /// The preset name.
        name: String,
    },
}

/// Registers the preset methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::MoaPresetsList, _, _>(catalog, list)?
        .register::<c::MoaPresetsPut, _, _>(catalog, put)?
        .register::<c::MoaPresetsDelete, _, _>(catalog, delete)?;
    Ok(())
}

async fn presets(
    call: &Call,
    profile: Option<&str>,
    change: Option<Change>,
) -> Result<c::MoaPresetList, RpcError> {
    let profile = profile_for(call, profile)?;
    call.server
        .host()
        .moa_presets(&profile, change)
        .await
        .map_err(|e| rpc(&e))
}

async fn list(call: Call, params: c::MoaPresetsListParams) -> Result<c::MoaPresetList, RpcError> {
    presets(&call, params.profile.as_deref(), None).await
}

async fn put(call: Call, params: c::MoaPresetsPutParams) -> Result<c::MoaPresetList, RpcError> {
    let change = Change::Put {
        name: params.name,
        preset: params.preset,
    };
    presets(&call, params.profile.as_deref(), Some(change)).await
}

async fn delete(
    call: Call,
    params: c::MoaPresetsDeleteParams,
) -> Result<c::MoaPresetList, RpcError> {
    let change = Change::Delete { name: params.name };
    presets(&call, params.profile.as_deref(), Some(change)).await
}
