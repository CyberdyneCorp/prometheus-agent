//! The session's prompt queue (spec: desktop-app, Composer history and
//! queue editing): `queue.list` for any attached client, the writers for
//! drivers. The host tells every attached client of a change
//! (`queue.changed`), so all windows show the same list.

use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::host::QueueOp;
use crate::methods::{Need, attached_profile, rpc};
use crate::router::Router;
use crate::server::Call;

/// Registers the queue methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::QueueList, _, _>(catalog, |call, p: c::SessionRef| {
            run(call, Need::Attached, QueueOp::List(p.session_id))
        })?
        .register::<c::QueueEdit, _, _>(catalog, |call, p: c::QueueEditParams| {
            let op = QueueOp::Edit {
                session_id: p.session_id,
                id: p.id,
                text: p.text,
            };
            run(call, Need::Driver, op)
        })?
        .register::<c::QueueDelete, _, _>(catalog, |call, p: c::QueueItemParams| {
            let op = QueueOp::Delete {
                session_id: p.session_id,
                id: p.id,
            };
            run(call, Need::Driver, op)
        })?
        .register::<c::QueuePause, _, _>(catalog, |call, p: c::SessionRef| {
            run(call, Need::Driver, QueueOp::Pause(p.session_id))
        })?
        .register::<c::QueueResume, _, _>(catalog, |call, p: c::SessionRef| {
            run(call, Need::Driver, QueueOp::Resume(p.session_id))
        })?;
    Ok(())
}

async fn run(call: Call, need: Need, op: QueueOp) -> Result<c::QueueState, RpcError> {
    let profile = attached_profile(&call, op.session_id(), need)?;
    let link = call.server.link(op.session_id());
    call.server
        .host()
        .queue(&profile, op, link)
        .await
        .map_err(|e| rpc(&e))
}
