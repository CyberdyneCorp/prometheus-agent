//! The built-in methods (spec: workspace-architecture, Catalog index of
//! capability-owned methods and events; Profile and ownership checks on
//! every session method; Event sequencing and reconnect replay). Every
//! method that reads or changes a session first checks that the session
//! belongs to a profile the connection's ticket grants and, when it
//! changes the session, that the connection is attached to it; both
//! failures are the same `-32002`, which never says whether the session
//! exists in another profile.

use pmt_core::error::{Error, codes};
use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::router::Router;
use crate::server::Call;

/// Registers every built-in method.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::Ping, _, _>(catalog, |_, _| async { Ok(c::Ack::YES) })?
        .register::<c::SessionCreate, _, _>(catalog, create)?
        .register::<c::SessionAttach, _, _>(catalog, attach)?
        .register::<c::SessionResume, _, _>(catalog, resume)?
        .register::<c::SessionInterrupt, _, _>(catalog, interrupt)?
        .register::<c::SessionResolve, _, _>(catalog, resolve)?
        .register::<c::SessionFork, _, _>(catalog, fork)?
        .register::<c::SessionDetach, _, _>(catalog, detach)?
        .register::<c::SessionListing, _, _>(catalog, list)?
        .register::<c::SessionEventsSince, _, _>(catalog, events_since)?
        .register::<c::SessionTranscript, _, _>(catalog, transcript)?
        .register::<c::SessionContextUsage, _, _>(catalog, context_usage)?
        .register::<c::SessionCompact, _, _>(catalog, compact)?
        .register::<c::PromptSubmit, _, _>(catalog, submit)?
        .register::<c::CommandDispatch, _, _>(catalog, dispatch)?
        .register::<c::ApprovalRespond, _, _>(catalog, approval_respond)?;
    // `rpc.hello` runs in the read loop (it must come first), but it is
    // registered so its contract is checked like every other result.
    router.register::<c::Hello, _, _>(catalog, |_, _| async {
        Err(RpcError::invalid_request("rpc.hello was already sent"))
    })?;
    Ok(())
}

fn rpc(err: &Error) -> RpcError {
    RpcError::from_error(err)
}

fn denied(session_id: &str) -> RpcError {
    rpc(&Error::new(
        &codes::RPC_SESSION_DENIED,
        format!("session {session_id} is not available to this connection"),
    ))
}

/// The profile a profile-scoped call acts on: the requested one when the
/// ticket grants it, else the only one it grants.
fn profile_for(call: &Call, requested: Option<&str>) -> Result<String, RpcError> {
    let granted = &call.conn.grant.profiles;
    match requested {
        Some(name) if granted.iter().any(|p| p == name) => Ok(name.to_owned()),
        Some(name) => Err(rpc(&Error::new(
            &codes::RPC_SESSION_DENIED,
            format!("profile '{name}' is not granted to this connection"),
        ))),
        None => match granted.as_slice() {
            [only] => Ok(only.clone()),
            _ => Err(RpcError::invalid_params(
                "/profile",
                false,
                "this connection serves several profiles; name one in `profile`",
            )),
        },
    }
}

/// The granted profile owning `session_id`; `mutating` also requires the
/// connection to be attached.
async fn access(call: &Call, session_id: &str, mutating: bool) -> Result<String, RpcError> {
    let server = &call.server;
    let profile = if let Some(known) = server.hub().profile_of(session_id) {
        call.conn.grant.profiles.contains(&known).then_some(known)
    } else {
        owner_among_granted(call, session_id).await?
    };
    let profile = profile.ok_or_else(|| denied(session_id))?;
    if mutating && !server.hub().is_attached(call.conn.id, session_id) {
        return Err(rpc(&Error::new(
            &codes::RPC_SESSION_DENIED,
            format!("attach to session {session_id} before changing it"),
        )));
    }
    Ok(profile)
}

/// The granted profile whose store has `session_id`, remembered by the hub.
async fn owner_among_granted(call: &Call, session_id: &str) -> Result<Option<String>, RpcError> {
    let server = &call.server;
    for profile in &call.conn.grant.profiles {
        if server
            .host()
            .owns(profile, session_id)
            .await
            .map_err(|e| rpc(&e))?
        {
            server.hub().register(session_id, profile);
            return Ok(Some(profile.clone()));
        }
    }
    Ok(None)
}

fn attached(call: &Call, session_id: &str) -> c::Attached {
    let latest_seq = call
        .server
        .hub()
        .attach(call.conn.id, session_id, call.conn.peer());
    c::Attached {
        session_id: session_id.to_owned(),
        latest_seq,
        epoch: call.server.hub().epoch().to_owned(),
    }
}

async fn create(call: Call, params: c::SessionCreateParams) -> Result<c::SessionRef, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    let exclusive = params.exclusive;
    let server = &call.server;
    let created = server
        .host()
        .create(&profile, params)
        .await
        .map_err(|e| rpc(&e))?;
    server.hub().register(&created.session_id, &profile);
    attached(&call, &created.session_id);
    if exclusive {
        claim(&call, &created.session_id)?;
    }
    Ok(created)
}

/// Makes the connection the session's interactive owner (spec: cli,
/// Single interactive owner per session).
fn claim(call: &Call, session_id: &str) -> Result<(), RpcError> {
    let pid = call.conn.grant.pid;
    call.server
        .hub()
        .claim(call.conn.id, session_id, pid)
        .map_err(|owner| {
            let owner = owner.map_or_else(|| "pid unknown".to_owned(), |p| format!("pid {p}"));
            rpc(&Error::new(
                &codes::SESSION_IN_USE,
                format!("session {session_id} is open in another client ({owner})"),
            )
            .with_hint("follow it read-only (session.attach) or fork it (session.fork)"))
        })
}

async fn attach(call: Call, params: c::SessionRef) -> Result<c::Attached, RpcError> {
    access(&call, &params.session_id, false).await?;
    Ok(attached(&call, &params.session_id))
}

async fn resume(call: Call, params: c::ResumeParams) -> Result<c::Attached, RpcError> {
    let profile = access(&call, &params.session_id, false).await?;
    if params.exclusive {
        claim(&call, &params.session_id)?;
    }
    let link = call.server.link(&params.session_id);
    call.server
        .host()
        .resume(&profile, params.setup, link)
        .await
        .map_err(|e| rpc(&e))?;
    Ok(attached(&call, &params.session_id))
}

async fn interrupt(call: Call, params: c::SessionRef) -> Result<c::Ack, RpcError> {
    let profile = access(&call, &params.session_id, true).await?;
    call.server
        .host()
        .interrupt(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))?;
    Ok(c::Ack::YES)
}

async fn resolve(call: Call, params: c::ResolveParams) -> Result<c::SessionRef, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    let found = call
        .server
        .host()
        .resolve(&profile, params)
        .await
        .map_err(|e| rpc(&e))?;
    call.server.hub().register(&found.session_id, &profile);
    Ok(found)
}

async fn fork(call: Call, params: c::ForkParams) -> Result<c::SessionRef, RpcError> {
    let profile = access(&call, &params.session_id, false).await?;
    let forked = call
        .server
        .host()
        .fork(&profile, params)
        .await
        .map_err(|e| rpc(&e))?;
    call.server.hub().register(&forked.session_id, &profile);
    Ok(forked)
}

async fn detach(call: Call, params: c::SessionRef) -> Result<c::Ack, RpcError> {
    access(&call, &params.session_id, true).await?;
    call.server.hub().detach(call.conn.id, &params.session_id);
    Ok(c::Ack::YES)
}

async fn list(call: Call, params: c::SessionListParams) -> Result<c::SessionList, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    call.server
        .host()
        .list(&profile, params)
        .await
        .map_err(|e| rpc(&e))
}

async fn events_since(
    call: Call,
    params: c::EventsSinceParams,
) -> Result<c::EventsSince, RpcError> {
    access(&call, &params.session_id, false).await?;
    Ok(call
        .server
        .hub()
        .events_since(&params.session_id, params.last_seen))
}

async fn transcript(call: Call, params: c::SessionRef) -> Result<c::Transcript, RpcError> {
    let profile = access(&call, &params.session_id, false).await?;
    call.server
        .host()
        .transcript(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))
}

async fn context_usage(call: Call, params: c::SessionRef) -> Result<c::ContextUsage, RpcError> {
    let profile = access(&call, &params.session_id, false).await?;
    call.server
        .host()
        .context_usage(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))
}

async fn compact(call: Call, params: c::CompactParams) -> Result<c::Compacted, RpcError> {
    let profile = access(&call, &params.session_id, true).await?;
    call.server
        .host()
        .compact(&profile, params)
        .await
        .map_err(|e| rpc(&e))
}

async fn submit(call: Call, params: c::SubmitParams) -> Result<c::Submitted, RpcError> {
    let profile = access(&call, &params.session_id, true).await?;
    let link = call.server.link(&params.session_id);
    call.server
        .host()
        .submit(&profile, params, link)
        .await
        .map_err(|e| rpc(&e))
}

async fn dispatch(call: Call, params: c::DispatchParams) -> Result<c::Dispatched, RpcError> {
    let profile = access(&call, &params.session_id, true).await?;
    let link = call.server.link(&params.session_id);
    call.server
        .host()
        .dispatch(&profile, params, link)
        .await
        .map_err(|e| rpc(&e))
}

async fn approval_respond(
    call: Call,
    params: c::ApprovalRespondParams,
) -> Result<c::Ack, RpcError> {
    let hub = call.server.hub();
    let session = hub.request_session(&params.id);
    if let Some(session) = &session {
        access(&call, session, true).await?;
    }
    let answer = c::ApprovalAnswer {
        decision: params.decision,
        accepted_hunks: params.accepted_hunks,
        replacement: params.replacement,
    };
    let value = serde_json::to_value(answer).unwrap_or_default();
    if session.is_some() && hub.resolve(call.conn.id, &params.id, Ok(value)) {
        return Ok(c::Ack::YES);
    }
    Err(rpc(&Error::new(
        &codes::RPC_REQUEST_CLOSED,
        format!("no open approval request {}", params.id),
    )))
}
