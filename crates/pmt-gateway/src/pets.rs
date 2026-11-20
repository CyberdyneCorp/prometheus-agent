//! Pets and achievements methods (spec: pets): `pets.*` and
//! `achievements.list` for the profile the call names (the connection's
//! grant must cover it). The host runs them; none touches a session.

use pmt_rpc::Catalog;
use pmt_rpc::contracts::{self as c, Method};
use serde_json::Value;

use crate::methods::{profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// One pets call: the method name and its params (as the contract parsed
/// them).
#[derive(Debug, Clone)]
pub struct PetsCall {
    /// The method name.
    pub method: &'static str,
    /// Its params.
    pub params: Value,
}

/// Registers the methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    add::<c::PetsStateGet>(router, catalog)?;
    add::<c::PetsSpriteGet>(router, catalog)?;
    add::<c::PetsGalleryList>(router, catalog)?;
    add::<c::PetsInstall>(router, catalog)?;
    add::<c::PetsSet>(router, catalog)?;
    add::<c::PetsHatchStart>(router, catalog)?;
    add::<c::PetsHatchAnswer>(router, catalog)?;
    add::<c::AchievementsList>(router, catalog)?;
    Ok(())
}

fn add<M: Method>(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router.register::<M, _, _>(catalog, |call: Call, params: M::Params| async move {
        let params = serde_json::to_value(&params).unwrap_or(Value::Null);
        let profile = profile_for(&call, params.get("profile").and_then(Value::as_str))?;
        let request = PetsCall {
            method: M::NAME,
            params,
        };
        let value = call
            .server
            .host()
            .pets(&profile, request)
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
