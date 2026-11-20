//! The TUI's M3 session methods (spec: tui, Split panes for subagents
//! and tool output; Session timeline scrubber): `subagent.subscribe`
//! streams a delegated child's transcript to the subscribing connection
//! as `subagent.event`s, `session.timeline` lists a session's turns and
//! `session.rewind` branches a new session off one of them. Each needs an
//! attachment to the session (`-32002 session.not_attached`).

use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{self as c, Event as _};
use pmt_rpc::{Catalog, RpcError};
use tokio::sync::mpsc;

use crate::host::{HostFuture, Link};
use crate::methods::{Need, attached_profile, rpc};
use crate::router::Router;
use crate::server::Call;

/// A child's transcript: what it did so far and what follows.
#[derive(Debug)]
pub struct SubagentFeed {
    /// `running`, or how it ended.
    pub status: String,
    /// Its transcript so far, oldest first.
    pub backlog: Vec<c::SubagentEventPayload>,
    /// What it does from now on; ends when the child has finished.
    pub live: Option<mpsc::UnboundedReceiver<c::SubagentEventPayload>>,
}

/// What these methods need from the daemon.
pub trait LiveHost: Send + Sync {
    /// The transcript feed of the session's child `subagent_id`.
    ///
    /// # Errors
    ///
    /// `delegation.not_in_tree` for a child of another session.
    fn subagent_feed<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        subagent_id: &'a str,
    ) -> HostFuture<'a, SubagentFeed>;

    /// The session's turns.
    fn timeline<'a>(&'a self, profile: &'a str, session_id: &'a str)
    -> HostFuture<'a, c::Timeline>;

    /// A new branch session ending at `params.to_turn`; `link` is the
    /// rewound session's (its workspace checkpoints are looked up).
    fn rewind<'a>(
        &'a self,
        profile: &'a str,
        params: c::RewindParams,
        link: Link,
    ) -> HostFuture<'a, c::SessionRewound>;
}

fn live(call: &Call) -> Result<&dyn LiveHost, RpcError> {
    call.server.host().live().ok_or_else(|| {
        rpc(&Error::new(
            &codes::RPC_NOT_SERVED,
            "this host serves no live session views",
        ))
    })
}

/// Registers the methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::SubagentSubscribe, _, _>(catalog, subscribe)?
        .register::<c::SessionTimeline, _, _>(catalog, timeline)?
        .register::<c::SessionRewind, _, _>(catalog, rewind)?;
    Ok(())
}

async fn subscribe(
    call: Call,
    params: c::SubagentSubscribeParams,
) -> Result<c::SubagentSubscribed, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    let feed = live(&call)?
        .subagent_feed(&profile, &params.session_id, &params.subagent_id)
        .await
        .map_err(|e| rpc(&e))?;
    let send = {
        let call = call.clone();
        let session = params.session_id.clone();
        move |item: &c::SubagentEventPayload| {
            let data = serde_json::to_value(item).unwrap_or_default();
            call.server.hub().notify_attached(
                call.conn.id,
                &session,
                c::SubagentStreamed::NAME,
                data,
            )
        }
    };
    for item in &feed.backlog {
        send(item);
    }
    if let Some(mut live) = feed.live {
        // Ends with the child, or once the connection is no longer
        // attached to the session.
        pmt_core::spawn::spawn(async move {
            while let Some(item) = live.recv().await {
                if !send(&item) {
                    break;
                }
            }
        });
    }
    Ok(c::SubagentSubscribed {
        subagent_id: params.subagent_id,
        status: feed.status,
    })
}

async fn timeline(call: Call, params: c::SessionRef) -> Result<c::Timeline, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    live(&call)?
        .timeline(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))
}

async fn rewind(call: Call, params: c::RewindParams) -> Result<c::SessionRewound, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    let link = call.server.link(&params.session_id);
    let rewound = live(&call)?
        .rewind(&profile, params, link)
        .await
        .map_err(|e| rpc(&e))?;
    call.server.hub().register(&rewound.session_id, &profile);
    Ok(rewound)
}
