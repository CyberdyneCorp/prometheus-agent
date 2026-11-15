//! The `evolve.*` and `harness.*` methods (spec: harness-evolution, Job
//! lifecycle, events and RPC). The host runs each as the matching
//! `prometheus evolve` or `prometheus harness` subcommand would, in the
//! daemon, whose evolution runner then runs started jobs.

use pmt_rpc::Catalog;
use pmt_rpc::contracts::{self as c, Method};

use crate::methods::{profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// One `evolve.*` or `harness.*` call: the method name and its params.
#[derive(Debug, Clone, PartialEq)]
pub struct EvolveRequest {
    /// The method name.
    pub method: &'static str,
    /// Its params.
    pub params: c::EvolveParams,
}

/// Registers the methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    add::<c::EvolveStart>(router, catalog)?;
    add::<c::EvolveStatus>(router, catalog)?;
    add::<c::EvolveList>(router, catalog)?;
    add::<c::EvolveCancel>(router, catalog)?;
    add::<c::EvolveChoose>(router, catalog)?;
    add::<c::EvolveProposalsList>(router, catalog)?;
    add::<c::EvolveProposalsAdopt>(router, catalog)?;
    add::<c::EvolveProposalsReject>(router, catalog)?;
    add::<c::HarnessLog>(router, catalog)?;
    add::<c::HarnessDiff>(router, catalog)?;
    add::<c::HarnessRollback>(router, catalog)?;
    Ok(())
}

fn add<M>(router: &mut Router, catalog: &Catalog) -> Result<(), String>
where
    M: Method<Params = c::EvolveParams>,
{
    router.register::<M, _, _>(catalog, |call: Call, params: c::EvolveParams| async move {
        let profile = profile_for(&call, params.profile.as_deref())?;
        let value = call
            .server
            .host()
            .evolve(
                &profile,
                EvolveRequest {
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
