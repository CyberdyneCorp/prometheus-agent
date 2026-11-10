//! The `local.*` methods (spec: local-models, Local models command): the
//! desktop's and dashboard's local models screens. The host runs each one
//! as the matching `prometheus local` subcommand would, in the daemon, so
//! a server it starts is supervised by the daemon.

use pmt_rpc::Catalog;
use pmt_rpc::contracts::{self as c, Method};

use crate::methods::{profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// One `local.*` call: the subcommand (`status`, `pull`...) and its params.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRequest {
    /// The method name without `local.`.
    pub op: &'static str,
    /// Its params.
    pub params: c::LocalParams,
}

/// Registers the `local.*` methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    add::<c::LocalStatus>(router, catalog)?;
    add::<c::LocalInstall>(router, catalog)?;
    add::<c::LocalUpdate>(router, catalog)?;
    add::<c::LocalCatalog>(router, catalog)?;
    add::<c::LocalPull>(router, catalog)?;
    add::<c::LocalPause>(router, catalog)?;
    add::<c::LocalResume>(router, catalog)?;
    add::<c::LocalJobs>(router, catalog)?;
    add::<c::LocalSearch>(router, catalog)?;
    add::<c::LocalAdd>(router, catalog)?;
    add::<c::LocalRm>(router, catalog)?;
    add::<c::LocalUse>(router, catalog)?;
    add::<c::LocalStart>(router, catalog)?;
    add::<c::LocalStop>(router, catalog)?;
    Ok(())
}

fn add<M>(router: &mut Router, catalog: &Catalog) -> Result<(), String>
where
    M: Method<Params = c::LocalParams, Result = c::LocalReply>,
{
    router.register::<M, _, _>(catalog, |call: Call, params: c::LocalParams| async move {
        let profile = profile_for(&call, params.profile.as_deref())?;
        let op = M::NAME.strip_prefix("local.").unwrap_or(M::NAME);
        call.server
            .host()
            .local(&profile, LocalRequest { op, params })
            .await
            .map_err(|e| rpc(&e))
    })?;
    Ok(())
}
