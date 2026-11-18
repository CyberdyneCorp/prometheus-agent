//! `screen.takeover` and `screen.handback` (spec: execution-environments,
//! Bot screen takeover lease): a viewer takes the profile's bot screen
//! from the agent, which then waits (`human_has_control`), and hands it
//! back; `screen.ticket` admits a viewer to the display stream and
//! `screen.browser` is the desktop's browser launcher on the screen. The
//! lease is the profile's `run/screen-lease.json`, shared with every
//! process of the profile ([`pmt_config::screen_lease`]).

use pmt_config::screen_lease::{self, Refused, ScreenLease, State};
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::{profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// Registers the screen methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::ScreenTakeover, _, _>(catalog, takeover)?
        .register::<c::ScreenHandback, _, _>(catalog, handback)?
        .register::<c::ScreenTicketCall, _, _>(catalog, ticket)?
        .register::<c::ScreenBrowser, _, _>(catalog, browser)?;
    Ok(())
}

/// The lease of `profile` on this daemon's home.
fn lease(call: &Call, requested: Option<&str>) -> Result<ScreenLease, RpcError> {
    let profile = profile_for(call, requested)?;
    let home = call.server.home().ok_or_else(|| {
        rpc(&Error::new(
            &codes::RPC_NOT_SERVED,
            "this daemon has no profiles on disk",
        ))
    })?;
    Ok(ScreenLease::of(&home.join("profiles").join(profile)))
}

/// Who a request speaks for: the name it gave, else its client kind.
fn viewer(call: &Call, named: Option<&str>) -> String {
    named
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map_or_else(
            || {
                call.conn
                    .client()
                    .map_or("viewer", |k| k.as_str())
                    .to_owned()
            },
            str::to_owned,
        )
}

fn reply(state: State) -> c::ScreenLeaseState {
    c::ScreenLeaseState {
        epoch: state.epoch,
        holder: state.holder.map(|h| h.viewer),
    }
}

async fn takeover(
    call: Call,
    params: c::ScreenTakeoverParams,
) -> Result<c::ScreenLeaseState, RpcError> {
    if call.conn.grant.observer_only {
        return Err(rpc(&Error::new(
            &codes::RPC_NOT_SERVED,
            "an observer cannot take the bot screen over",
        )));
    }
    let lease = lease(&call, params.profile.as_deref())?;
    let viewer = viewer(&call, params.viewer.as_deref());
    let state = lease
        .take(&viewer, screen_lease::now())
        .map_err(|e| rpc(&e))?;
    Ok(reply(state))
}

async fn handback(
    call: Call,
    params: c::ScreenHandbackParams,
) -> Result<c::ScreenLeaseState, RpcError> {
    let lease = lease(&call, params.profile.as_deref())?;
    let viewer = viewer(&call, params.viewer.as_deref());
    match lease
        .hand_back(&viewer, params.force, screen_lease::now())
        .map_err(|e| rpc(&e))?
    {
        Ok(state) => Ok(reply(state)),
        Err(Refused::HeldBy(holder)) => Err(rpc(&Error::new(
            &codes::SCREEN_HUMAN_HAS_CONTROL,
            format!("{holder} holds the bot screen; hand it back with force to take it from them"),
        ))),
    }
}

/// A single-use display ticket for the profile's screen stream, with who
/// controls the screen now; an observer gets one too (watching is not
/// taking over).
async fn ticket(call: Call, params: c::ScreenTicketParams) -> Result<c::ScreenTicket, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    let state = lease(&call, Some(profile.as_str()))?.read(screen_lease::now());
    let grant = crate::tickets::Grant {
        profiles: vec![profile],
        purpose: pmt_rpc::control::Purpose::Display,
        pid: None,
        observer_only: call.conn.grant.observer_only,
        admin: false,
    };
    let issued = call
        .server
        .tickets
        .issue(grant, &call.server.profiles())
        .map_err(|e| rpc(&e))?;
    Ok(c::ScreenTicket {
        ticket: issued.ticket,
        path: crate::screen_stream::PATH.to_owned(),
        expires_in_secs: issued.expires_in_secs,
        lease: reply(state),
    })
}

/// Opens a browser on the profile's screen; an observer cannot.
async fn browser(
    call: Call,
    params: c::ScreenBrowserParams,
) -> Result<c::ScreenBrowserOpened, RpcError> {
    if call.conn.grant.observer_only {
        return Err(rpc(&Error::new(
            &codes::RPC_NOT_SERVED,
            "an observer cannot open a browser on the bot screen",
        )));
    }
    let profile = profile_for(&call, params.profile.as_deref())?;
    crate::panes::panes(&call)?
        .screen_browser(&profile, params.url.as_deref())
        .await
        .map_err(|e| rpc(&e))
}
