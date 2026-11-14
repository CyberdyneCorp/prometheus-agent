//! Terminal methods (spec: desktop-app, Embedded terminals are
//! daemon-owned PTYs) and the live subagents frame's controls (Live
//! subagents frame). Only drivers open or write to a terminal, and only
//! the one holding it (the last driver that opened or attached to it);
//! any attached client lists one and reads its recent output.

use pmt_core::error::{Error, codes};
use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::{Need, attached_profile, rpc};
use crate::router::Router;
use crate::server::Call;

/// `-32002`: an observer typed into a terminal.
pub(crate) fn observer_refusal() -> RpcError {
    rpc(&Error::new(
        &codes::SESSION_OBSERVER,
        "observers cannot write to terminals",
    ))
}

/// The session's profile for a terminal writer: drivers only.
fn driver(call: &Call, session_id: &str) -> Result<String, RpcError> {
    let profile = attached_profile(call, session_id, Need::Attached)?;
    let role = call.server.hub().role(call.conn.id, session_id);
    if role == Some(c::AttachRole::Observer) {
        return Err(observer_refusal());
    }
    Ok(profile)
}

/// Registers the terminal and subagent control methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::PtyOpen, _, _>(catalog, open)?
        .register::<c::PtyAttach, _, _>(catalog, attach)?
        .register::<c::PtyListCall, _, _>(catalog, |call, p: c::SessionRef| async move {
            attached_profile(&call, &p.session_id, Need::Attached)?;
            Ok(c::PtyList {
                terminals: call.server.terminals().of_session(&p.session_id),
            })
        })?
        .register::<c::PtyInput, _, _>(catalog, |call, p: c::PtyInputParams| async move {
            holding(&call, &p.session_id, &p.pty_id)?;
            call.server
                .terminals()
                .input(&p.session_id, &p.pty_id, p.data.as_bytes())
                .map_err(|e| rpc(&e))?;
            Ok(c::Ack::YES)
        })?
        .register::<c::PtyResize, _, _>(catalog, |call, p: c::PtyResizeParams| async move {
            holding(&call, &p.session_id, &p.pty_id)?;
            call.server
                .terminals()
                .resize(&p.session_id, &p.pty_id, (p.cols, p.rows))
                .map_err(|e| rpc(&e))?;
            Ok(c::Ack::YES)
        })?
        .register::<c::PtyClose, _, _>(catalog, |call, p: c::PtyRef| async move {
            driver(&call, &p.session_id)?;
            call.server
                .terminals()
                .close(&p.session_id, &p.pty_id)
                .map_err(|e| rpc(&e))?;
            Ok(c::Ack::YES)
        })?
        .register::<c::SubagentSteer, _, _>(
            catalog,
            |call, p: c::SubagentSteerParams| async move {
                control(&call, &p.session_id, &p.subagent_id, Some(&p.text)).await
            },
        )?
        .register::<c::SubagentStop, _, _>(catalog, |call, p: c::SubagentRef| async move {
            control(&call, &p.session_id, &p.subagent_id, None).await
        })?;
    Ok(())
}

async fn open(call: Call, params: c::PtyOpenParams) -> Result<c::PtyOpened, RpcError> {
    let profile = driver(&call, &params.session_id)?;
    let cwd = call
        .server
        .host()
        .terminal_cwd(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))?;
    let hub = std::sync::Arc::clone(call.server.hub());
    let terminals = call.server.terminals();
    let pty_id = terminals
        .open(&hub, &params.session_id, &cwd, (params.cols, params.rows))
        .map_err(|e| rpc(&e))?;
    terminals
        .hold(&params.session_id, &pty_id, call.conn.id)
        .map_err(|e| rpc(&e))?;
    Ok(c::PtyOpened { pty_id })
}

/// `pty.attach`: the recent output; a driver takes the terminal over and
/// the connection that held it is told (`pty.detached`).
async fn attach(call: Call, p: c::PtyRef) -> Result<c::PtyAttached, RpcError> {
    attached_profile(&call, &p.session_id, Need::Attached)?;
    let terminals = call.server.terminals();
    let replay = terminals
        .replay(&p.session_id, &p.pty_id)
        .map_err(|e| rpc(&e))?;
    let role = call.server.hub().role(call.conn.id, &p.session_id);
    if role != Some(c::AttachRole::Observer)
        && let Some(before) = terminals
            .hold(&p.session_id, &p.pty_id, call.conn.id)
            .map_err(|e| rpc(&e))?
    {
        let detached = serde_json::json!({"session_id": p.session_id, "pty_id": p.pty_id});
        call.server
            .hub()
            .notify_attached(before, &p.session_id, "pty.detached", detached);
    }
    Ok(c::PtyAttached {
        pty_id: p.pty_id,
        replay,
    })
}

/// A driver that may type into the terminal: its holder, or any driver
/// once the holder left the session (it then holds it).
fn holding(call: &Call, session_id: &str, pty_id: &str) -> Result<(), RpcError> {
    driver(call, session_id)?;
    let terminals = call.server.terminals();
    let holder = terminals.holder(session_id, pty_id).map_err(|e| rpc(&e))?;
    let conn = call.conn.id;
    if let Some(other) = holder.filter(|h| *h != conn)
        && call.server.hub().is_attached(other, session_id)
    {
        return Err(rpc(&Error::new(
            &codes::TERMINAL_TAKEN,
            "another window or tab took this terminal over; attach to it again to take it back",
        )));
    }
    terminals
        .hold(session_id, pty_id, conn)
        .map_err(|e| rpc(&e))?;
    Ok(())
}

async fn control(
    call: &Call,
    session_id: &str,
    subagent_id: &str,
    steer: Option<&str>,
) -> Result<c::SubagentControlled, RpcError> {
    let profile = attached_profile(call, session_id, Need::Driver)?;
    let status = call
        .server
        .host()
        .subagent_control(&profile, session_id, subagent_id, steer)
        .await
        .map_err(|e| rpc(&e))?;
    Ok(c::SubagentControlled {
        subagent_id: subagent_id.to_owned(),
        status,
    })
}
