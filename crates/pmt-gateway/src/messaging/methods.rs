//! The messaging gateway's RPC methods (spec: messaging-gateway,
//! Documented pairing flow): `pairing.approve` lets a client attached to
//! a profile approve one of its pending pairing codes.

use pmt_core::error::{Error, codes};
use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::{profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// Registers the messaging methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub(crate) fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router.register::<c::PairingApprove, _, _>(catalog, approve)?;
    Ok(())
}

async fn approve(
    call: Call,
    params: c::PairingApproveParams,
) -> Result<c::PairingApproved, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    let Some(messaging) = call.server.messaging() else {
        let err = Error::new(
            &codes::RPC_NOT_SERVED,
            "this gateway runs no messaging adapters",
        );
        return Err(rpc(&err));
    };
    let pairing = messaging
        .approve_pairing(&profile, &params.platform, &params.code)
        .map_err(|e| rpc(&e))?;
    Ok(c::PairingApproved {
        platform: pairing.platform,
        sender_id: pairing.sender_id,
        sender_name: pairing.sender_name,
    })
}
