//! The `experience.*` methods (spec: experience-library, Library CLI).
//! The host runs each as the matching `prometheus experience` subcommand
//! would.

use pmt_rpc::Catalog;
use pmt_rpc::contracts::{self as c, Method};

use crate::methods::{profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// One `experience.*` call: the method name and its params.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperienceRequest {
    /// The method name.
    pub method: &'static str,
    /// Its params.
    pub params: c::ExperienceParams,
}

/// Registers the methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    add::<c::ExperienceList>(router, catalog)?;
    add::<c::ExperienceShow>(router, catalog)?;
    add::<c::ExperienceDomainsList>(router, catalog)?;
    add::<c::ExperiencePin>(router, catalog)?;
    add::<c::ExperienceForget>(router, catalog)?;
    add::<c::ExperienceDisable>(router, catalog)?;
    add::<c::ExperienceRestore>(router, catalog)?;
    Ok(())
}

fn add<M>(router: &mut Router, catalog: &Catalog) -> Result<(), String>
where
    M: Method<Params = c::ExperienceParams>,
{
    router.register::<M, _, _>(
        catalog,
        |call: Call, params: c::ExperienceParams| async move {
            let profile = profile_for(&call, params.profile.as_deref())?;
            let value = call
                .server
                .host()
                .experience(
                    &profile,
                    ExperienceRequest {
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
        },
    )?;
    Ok(())
}
