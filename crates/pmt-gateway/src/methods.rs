//! The built-in methods (spec: workspace-architecture, Catalog index of
//! capability-owned methods and events; Profile and ownership checks on
//! every session method; Event sequencing and reconnect replay; tui,
//! Session access requires an attachment; Idempotent session creation;
//! Turn authors are stamped by the gateway; Profile scope on every
//! request).
//!
//! Attaching (`session.attach`, `session.resume`) checks that the session
//! belongs to a profile the connection's ticket grants, answering the same
//! `-32002` whether it exists elsewhere or not at all. Every other
//! session-addressed method then needs that attachment (`-32002
//! session.not_attached`), and the methods that drive the session need it
//! as a driver (`-32002 session.observer`). Profile-scoped methods act on
//! the named profile, else the connection's only one.

use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};
use tokio::time::Instant;

use crate::host::Author;
use crate::hub::{Answered, Since};
use crate::router::Router;
use crate::server::Call;

/// How long a `session.create` idempotency key holds.
const IDEMPOTENCY_WINDOW: Duration = Duration::from_mins(5);

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
        .register::<c::SessionSteer, _, _>(catalog, steer)?
        .register::<c::SessionResolve, _, _>(catalog, resolve)?
        .register::<c::SessionFork, _, _>(catalog, fork)?
        .register::<c::SessionTree, _, _>(catalog, tree)?
        .register::<c::SessionSiblings, _, _>(catalog, siblings)?
        .register::<c::SessionDetach, _, _>(catalog, detach)?
        .register::<c::SessionListing, _, _>(catalog, list)?
        .register::<c::SessionEventsSince, _, _>(catalog, events_since)?
        .register::<c::SessionTranscript, _, _>(catalog, transcript)?
        .register::<c::SessionContextUsage, _, _>(catalog, context_usage)?
        .register::<c::SessionCompact, _, _>(catalog, compact)?
        .register::<c::EgressSet, _, _>(catalog, egress)?
        .register::<c::FilesUndo, _, _>(catalog, files_undo)?
        .register::<c::FilesRollback, _, _>(catalog, files_rollback)?
        .register::<c::LspStatusCall, _, _>(catalog, lsp_status)?
        .register::<c::BlueprintsListCall, _, _>(catalog, blueprints_list)?
        .register::<c::BlueprintsFillCall, _, _>(catalog, blueprints_fill)?
        .register::<c::PromptSubmit, _, _>(catalog, submit)?
        .register::<c::CommandDispatch, _, _>(catalog, dispatch)?
        .register::<c::ApprovalRespond, _, _>(catalog, approval_respond)?;
    crate::discovery::register(router, catalog)?;
    crate::desktop::register(router, catalog)?;
    crate::queue::register(router, catalog)?;
    crate::workspace::register(router, catalog)?;
    crate::ui_prefs::register(router, catalog)?;
    crate::vault::register(router, catalog)?;
    crate::moa::register(router, catalog)?;
    crate::observability::register(router, catalog)?;
    crate::local::register(router, catalog)?;
    crate::upkeep::register(router, catalog)?;
    crate::worktree::register(router, catalog)?;
    crate::messaging::methods::register(router, catalog)?;
    // `rpc.hello` runs in the read loop (it must come first), but it is
    // registered so its contract is checked like every other result.
    router.register::<c::Hello, _, _>(catalog, |_, _| async {
        Err(RpcError::invalid_request("rpc.hello was already sent"))
    })?;
    Ok(())
}

pub(crate) fn rpc(err: &Error) -> RpcError {
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
pub(crate) fn profile_for(call: &Call, requested: Option<&str>) -> Result<String, RpcError> {
    let granted = &call.conn.grant.profiles;
    match requested {
        Some(name) if granted.iter().any(|p| p == name) => Ok(name.to_owned()),
        Some(name) => Err(rpc(&Error::new(
            &codes::PROFILE_NOT_GRANTED,
            format!("profile {name} is not granted to this connection"),
        ))),
        None => match granted.as_slice() {
            [only] => Ok(only.clone()),
            _ => Err(RpcError::invalid_params(
                "/profile",
                false,
                "profile is required: this connection is granted several profiles",
            )),
        },
    }
}

/// What a session-addressed method needs of the connection's attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Need {
    /// Any role.
    Attached,
    /// A driver: it submits, interrupts, changes or answers.
    Driver,
}

/// The profile of `session_id` when the connection is attached to it
/// (as a driver for [`Need::Driver`]).
pub(crate) fn attached_profile(
    call: &Call,
    session_id: &str,
    need: Need,
) -> Result<String, RpcError> {
    let hub = call.server.hub();
    let role = hub.role(call.conn.id, session_id);
    let profile = hub
        .profile_of(session_id)
        .filter(|p| call.conn.grant.profiles.contains(p));
    let (Some(role), Some(profile)) = (role, profile) else {
        return Err(rpc(&Error::new(
            &codes::SESSION_NOT_ATTACHED,
            format!("session {session_id} is not attached on this connection"),
        )
        .with_hint("attach first: session.attach {session_id}")));
    };
    if need == Need::Driver && role == c::AttachRole::Observer {
        return Err(rpc(&Error::new(
            &codes::SESSION_OBSERVER,
            "observers cannot submit input",
        )));
    }
    Ok(profile)
}

/// The granted profile owning `session_id` (attaching, resuming).
async fn owner(call: &Call, session_id: &str) -> Result<String, RpcError> {
    let server = &call.server;
    if let Some(known) = server.hub().profile_of(session_id) {
        return if call.conn.grant.profiles.contains(&known) {
            Ok(known)
        } else {
            Err(denied(session_id))
        };
    }
    for profile in &call.conn.grant.profiles {
        if server
            .host()
            .owns(profile, session_id)
            .await
            .map_err(|e| rpc(&e))?
        {
            server.hub().register(session_id, profile);
            return Ok(profile.clone());
        }
    }
    // A connection granted every served profile has looked everywhere:
    // "not found" then discloses nothing about other profiles (spec:
    // desktop-app, Gone-session verdict).
    let granted = &call.conn.grant.profiles;
    if server.profiles().iter().all(|p| granted.contains(p)) {
        return Err(rpc(&Error::new(
            &codes::SESSION_NOT_FOUND,
            format!("session {session_id} not found"),
        )));
    }
    Err(denied(session_id))
}

/// Attaches the connection; a connection granted observation only (a
/// remote dashboard user who is not a driver) attaches as an observer
/// whatever it asked for.
fn join(call: &Call, session_id: &str, role: c::AttachRole, since: Option<&Since>) -> c::Attached {
    let role = if call.conn.grant.observer_only {
        c::AttachRole::Observer
    } else {
        role
    };
    call.server
        .hub()
        .attach(call.conn.id, session_id, call.conn.peer(role), since)
}

async fn create(call: Call, params: c::SessionCreateParams) -> Result<c::SessionRef, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    if let Some(source) = params.source
        && Some(source) != call.conn.client()
    {
        return Err(rpc(&Error::new(
            &codes::RPC_SOURCE_MISMATCH,
            format!(
                "session.create source {} is not this connection's client kind",
                source.as_str()
            ),
        )));
    }
    let mut params = params;
    // The session's source is the connection's own client kind.
    params.source = call.conn.client();
    let exclusive = params.exclusive;
    let created = match params.idempotency_key.clone() {
        Some(key) => create_once(&call, &profile, key, params).await?,
        None => fresh(&call, &profile, params).await?,
    };
    join(&call, &created.session_id, c::AttachRole::Driver, None);
    if exclusive {
        claim(&call, &created.session_id)?;
    }
    Ok(created)
}

async fn fresh(
    call: &Call,
    profile: &str,
    params: c::SessionCreateParams,
) -> Result<c::SessionRef, RpcError> {
    let server = &call.server;
    let created = server
        .host()
        .create(profile, params)
        .await
        .map_err(|e| rpc(&e))?;
    server.hub().register(&created.session_id, profile);
    Ok(created)
}

/// A keyed create: the session an earlier create with the same key made
/// within the window, else a new one. Keyed creates of a daemon run one
/// at a time, so two concurrent retries cannot both create.
async fn create_once(
    call: &Call,
    profile: &str,
    key: String,
    params: c::SessionCreateParams,
) -> Result<c::SessionRef, RpcError> {
    let mut created = call.server.created.lock().await;
    created.retain(|_, (_, at)| at.elapsed() < IDEMPOTENCY_WINDOW);
    let slot = (profile.to_owned(), key);
    if let Some((session_id, _)) = created.get(&slot) {
        return Ok(c::SessionRef {
            session_id: session_id.clone(),
        });
    }
    let fresh = fresh(call, profile, params).await?;
    created.insert(slot, (fresh.session_id.clone(), Instant::now()));
    Ok(fresh)
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

async fn attach(call: Call, params: c::AttachParams) -> Result<c::Attached, RpcError> {
    owner(&call, &params.session_id).await?;
    let since = params.last_seen_seq.map(|last_seen| Since {
        last_seen,
        epoch: params.epoch.clone(),
    });
    Ok(join(&call, &params.session_id, params.role, since.as_ref()))
}

async fn resume(call: Call, params: c::ResumeParams) -> Result<c::Attached, RpcError> {
    let profile = owner(&call, &params.session_id).await?;
    if params.exclusive {
        claim(&call, &params.session_id)?;
    }
    let link = call.server.link(&params.session_id);
    call.server
        .host()
        .resume(&profile, params.setup, link)
        .await
        .map_err(|e| rpc(&e))?;
    Ok(join(&call, &params.session_id, c::AttachRole::Driver, None))
}

async fn interrupt(call: Call, params: c::SessionRef) -> Result<c::Ack, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Driver)?;
    call.server
        .host()
        .interrupt(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))?;
    Ok(c::Ack::YES)
}

async fn steer(call: Call, params: c::SteerParams) -> Result<c::Ack, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Driver)?;
    call.server
        .host()
        .steer_session(&profile, params)
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
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    let forked = call
        .server
        .host()
        .fork(&profile, params)
        .await
        .map_err(|e| rpc(&e))?;
    call.server.hub().register(&forked.session_id, &profile);
    Ok(forked)
}

async fn tree(call: Call, params: c::SessionRef) -> Result<c::Lineage, RpcError> {
    lineage(call, params, false).await
}

async fn siblings(call: Call, params: c::SessionRef) -> Result<c::Lineage, RpcError> {
    lineage(call, params, true).await
}

async fn lineage(
    call: Call,
    params: c::SessionRef,
    siblings: bool,
) -> Result<c::Lineage, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    call.server
        .host()
        .lineage(&profile, &params.session_id, siblings)
        .await
        .map_err(|e| rpc(&e))
}

async fn detach(call: Call, params: c::SessionRef) -> Result<c::Ack, RpcError> {
    attached_profile(&call, &params.session_id, Need::Attached)?;
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
    attached_profile(&call, &params.session_id, Need::Attached)?;
    Ok(call
        .server
        .hub()
        .events_since(&params.session_id, params.last_seen))
}

async fn transcript(call: Call, params: c::SessionRef) -> Result<c::Transcript, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    call.server
        .host()
        .transcript(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))
}

async fn context_usage(call: Call, params: c::SessionRef) -> Result<c::ContextUsage, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    call.server
        .host()
        .context_usage(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))
}

async fn egress(call: Call, params: c::EgressSetParams) -> Result<c::EgressState, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Driver)?;
    let link = call.server.link(&params.session_id);
    call.server
        .host()
        .egress(&profile, params, link)
        .await
        .map_err(|e| rpc(&e))
}

async fn lsp_status(call: Call, params: c::LspStatusParams) -> Result<c::LspStatus, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    call.server.host().lsp_status(&profile).map_err(|e| rpc(&e))
}

async fn blueprints_list(
    call: Call,
    params: c::BlueprintsListParams,
) -> Result<c::BlueprintList, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    call.server
        .host()
        .cron_blueprints(&profile)
        .map_err(|e| rpc(&e))
}

async fn blueprints_fill(
    call: Call,
    params: c::BlueprintFillParams,
) -> Result<c::BlueprintFilled, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    call.server
        .host()
        .cron_blueprint_fill(&profile, &params)
        .map_err(|e| rpc(&e))
}

async fn files_undo(call: Call, params: c::FilesUndoParams) -> Result<c::FilesUndone, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Driver)?;
    let link = call.server.link(&params.session_id);
    call.server
        .host()
        .files_undo(&profile, params, link)
        .await
        .map_err(|e| rpc(&e))
}

async fn files_rollback(
    call: Call,
    params: c::FilesRollbackParams,
) -> Result<c::FilesRolledBack, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Driver)?;
    let link = call.server.link(&params.session_id);
    call.server
        .host()
        .files_rollback(&profile, params, link)
        .await
        .map_err(|e| rpc(&e))
}

async fn compact(call: Call, params: c::CompactParams) -> Result<c::Compacted, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Driver)?;
    call.server
        .host()
        .compact(&profile, params)
        .await
        .map_err(|e| rpc(&e))
}

async fn submit(call: Call, params: c::SubmitParams) -> Result<c::Submitted, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Driver)?;
    if params.author.is_some() {
        return Err(rpc(&Error::new(
            &codes::RPC_FORGED_AUTHOR,
            "turn author is stamped by the gateway, never by a client",
        )));
    }
    let author = Author {
        client_kind: call
            .conn
            .client()
            .unwrap_or(c::ClientKind::Cli)
            .as_str()
            .to_owned(),
        client_id: call.conn.client_id(),
        principal: profile.clone(),
        sent_at: None,
        prefix: None,
        origin: None,
    };
    let Some(input_id) = params.input_id.clone() else {
        return admit(&call, &profile, params, author).await;
    };
    // Admissions of inputs with an id run one at a time, so a resend
    // racing its original cannot start a second turn.
    let mut admitted = call.server.admitted.lock().await;
    let key = (params.session_id.clone(), input_id);
    if let Some((_, original)) = admitted.iter().find(|(k, _)| *k == key) {
        return Ok(original.clone());
    }
    let submitted = admit(&call, &profile, params, author).await?;
    if admitted.len() == ADMISSIONS_KEPT {
        admitted.pop_front();
    }
    admitted.push_back((key, submitted.clone()));
    Ok(submitted)
}

/// How many admissions are remembered for resent inputs.
const ADMISSIONS_KEPT: usize = 4096;

async fn admit(
    call: &Call,
    profile: &str,
    params: c::SubmitParams,
    author: Author,
) -> Result<c::Submitted, RpcError> {
    let link = call.server.link(&params.session_id);
    call.server
        .host()
        .submit(profile, params, author, link)
        .await
        .map_err(|e| rpc(&e))
}

async fn dispatch(call: Call, params: c::DispatchParams) -> Result<c::Dispatched, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Driver)?;
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
    if let Some(session) = hub.request_session(&params.id) {
        attached_profile(&call, &session, Need::Driver)?;
    }
    let answer = c::ApprovalAnswer {
        decision: params.decision,
        accepted_hunks: params.accepted_hunks,
        replacement: params.replacement,
    };
    let value = serde_json::to_value(answer).unwrap_or_default();
    match hub.resolve(call.conn.id, &call.conn.client_id(), &params.id, Ok(value)) {
        Answered::Taken => Ok(c::Ack::YES),
        Answered::Settled => Err(rpc(&Error::new(
            &codes::RPC_REQUEST_SETTLED,
            format!("request {} already settled", params.id),
        ))),
        Answered::Unknown => Err(rpc(&Error::new(
            &codes::RPC_REQUEST_CLOSED,
            format!("no open approval request {}", params.id),
        ))),
    }
}
