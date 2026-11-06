//! `worktree.new`, `worktree.status`, `worktree.list` and `worktree.prune`
//! (spec: git-worktrees, In-session worktree command): the host runs them
//! on the session a client drives, exactly as `/worktree` does.

use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::{Need, attached_profile, rpc};
use crate::router::Router;
use crate::server::Call;

/// One worktree request, by method.
#[derive(Debug, Clone)]
pub enum WorktreeCall {
    /// `worktree.new`.
    New(c::WorktreeNewParams),
    /// `worktree.status`.
    Status(c::WorktreeSessionParams),
    /// `worktree.list`.
    List(c::WorktreeSessionParams),
    /// `worktree.prune`.
    Prune(c::WorktreePruneParams),
}

impl WorktreeCall {
    /// The session it names.
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::New(p) => &p.session_id,
            Self::Status(p) | Self::List(p) => &p.session_id,
            Self::Prune(p) => &p.session_id,
        }
    }
}

/// Its answer.
#[derive(Debug, Clone)]
pub enum WorktreeReply {
    /// `worktree.new`.
    Created(c::WorktreeCreated),
    /// `worktree.status`.
    Status(c::WorktreeStatus),
    /// `worktree.list`.
    Listing(c::WorktreeListing),
    /// `worktree.prune`.
    Pruned(c::WorktreePruned),
}

/// Registers the worktree methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::WorktreeNew, _, _>(catalog, new)?
        .register::<c::WorktreeStatusCall, _, _>(catalog, status)?
        .register::<c::WorktreeList, _, _>(catalog, list)?
        .register::<c::WorktreePrune, _, _>(catalog, prune)?;
    Ok(())
}

async fn run(call: &Call, request: WorktreeCall, need: Need) -> Result<WorktreeReply, RpcError> {
    let profile = attached_profile(call, request.session_id(), need)?;
    let link = call.server.link(request.session_id());
    call.server
        .host()
        .worktree(&profile, request, link)
        .await
        .map_err(|e| rpc(&e))
}

fn unexpected() -> RpcError {
    rpc(&pmt_core::error::Error::new(
        &pmt_core::error::codes::RPC_NOT_SERVED,
        "the host answered a different worktree request",
    ))
}

async fn new(call: Call, params: c::WorktreeNewParams) -> Result<c::WorktreeCreated, RpcError> {
    match run(&call, WorktreeCall::New(params), Need::Driver).await? {
        WorktreeReply::Created(created) => Ok(created),
        _ => Err(unexpected()),
    }
}

async fn status(
    call: Call,
    params: c::WorktreeSessionParams,
) -> Result<c::WorktreeStatus, RpcError> {
    match run(&call, WorktreeCall::Status(params), Need::Attached).await? {
        WorktreeReply::Status(status) => Ok(status),
        _ => Err(unexpected()),
    }
}

async fn list(
    call: Call,
    params: c::WorktreeSessionParams,
) -> Result<c::WorktreeListing, RpcError> {
    match run(&call, WorktreeCall::List(params), Need::Attached).await? {
        WorktreeReply::Listing(listing) => Ok(listing),
        _ => Err(unexpected()),
    }
}

async fn prune(call: Call, params: c::WorktreePruneParams) -> Result<c::WorktreePruned, RpcError> {
    match run(&call, WorktreeCall::Prune(params), Need::Driver).await? {
        WorktreeReply::Pruned(pruned) => Ok(pruned),
        _ => Err(unexpected()),
    }
}
