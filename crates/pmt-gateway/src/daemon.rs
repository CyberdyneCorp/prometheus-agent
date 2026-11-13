//! The daemon's lifetime (spec: workspace-architecture, One daemon per
//! Prometheus home; Daemon discovery file and local control channel;
//! Stale daemon build detection; messaging-gateway, One gateway daemon per
//! home): bind the loopback RPC listener and the control channel, publish
//! the endpoint in `run/gateway.json`, serve until a stop is requested,
//! drain running turns to a boundary, then close every connection with
//! 1001. The caller holds the home's writer lock throughout, so a second
//! daemon fails before it opens anything.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_core::runtime::WriterLock;
use pmt_net::listen::{Exposure, bind};
use pmt_rpc::discovery::Endpoint;
use tracing::Instrument as _;

use crate::control;
use crate::server::Server;

/// How the daemon stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// A requested graceful stop (exit 0).
    Stopped,
    /// Stopped to be replaced (`daemon restart`; exit 75 so a supervisor
    /// restarts it).
    Restart,
    /// Every messaging adapter failed for good under
    /// `gateway.on_all_adapters_down = "exit"` (exit 78, no restart).
    AdaptersDown,
}

/// Serves `server` for the home whose writer lock `lock` is, until a stop
/// is requested over the control channel or `shutdown` completes; running
/// turns get `drain` to finish.
///
/// # Errors
///
/// `net.listen_refused` / `net.listen` when the RPC listener cannot be
/// bound, `home.io` for the control channel or the discovery file.
pub async fn run(
    lock: &WriterLock,
    home: &Path,
    server: Arc<Server>,
    drain: Duration,
    shutdown: impl Future<Output = ()>,
) -> Result<Exit, Error> {
    let addr = server
        .rpc_config()
        .bind_addr()
        .map_err(|message| Error::new(&codes::CONFIG_PARSE, message))?;
    let listener = bind("daemon rpc", addr, Exposure::default()).await?;
    let local = listener
        .local_addr()
        .map_err(|e| Error::new(&codes::NET_LISTEN, format!("daemon rpc: {e}")))?;
    let address = control::address(home)?;
    let control = control::Listener::bind(&address)?;
    let endpoint = Endpoint {
        instance_id: server.instance_id().to_owned(),
        build_id: server.identity().build_id.clone(),
        rpc_url: format!("ws://{local}/rpc"),
        control: address,
        rpc_protocol: server.protocol().to_string(),
        started_at: server.started_at().to_owned(),
        health_url: server.health_url().map(str::to_owned),
        features: pmt_rpc::discovery::FEATURES
            .iter()
            .map(|f| (*f).to_owned())
            .collect(),
    };
    server.set_endpoint(endpoint.clone());
    let control_task = pmt_core::spawn::spawn(control.serve(Arc::clone(&server)).in_current_span());
    let accept_task =
        pmt_core::spawn::spawn(accept(listener, Arc::clone(&server)).in_current_span());
    let reap_task = pmt_core::spawn::spawn(reap(Arc::clone(&server)).in_current_span());
    lock.publish(&endpoint)?;
    tracing::info!(rpc_url = %endpoint.rpc_url, "daemon ready");
    tokio::select! {
        () = server.stop.cancelled() => {}
        () = shutdown => server.request_stop(false),
    }
    if let Some(messaging) = server.messaging() {
        messaging.shutdown().await;
    }
    server.host().drain(drain).await;
    server.closing.cancel();
    let _ = accept_task.await;
    reap_task.abort();
    let _ = control_task.await;
    wait_for_connections(&server).await;
    Ok(if server.adapters_down() {
        Exit::AdaptersDown
    } else if server.restart_requested() {
        Exit::Restart
    } else {
        Exit::Stopped
    })
}

async fn accept(listener: tokio::net::TcpListener, server: Arc<Server>) {
    loop {
        tokio::select! {
            () = server.closing.cancelled() => return,
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    pmt_core::spawn::spawn(crate::connection::serve(Arc::clone(&server), stream).in_current_span());
                }
            }
        }
    }
}

/// Closes sessions no client has attached to for
/// `daemon.idle_session_close_secs` and that run nothing (spec: tui,
/// Orphaned attachments are reaped). A session with a running turn or a
/// queued input is left to finish and retried on the next sweep.
async fn reap(server: Arc<Server>) {
    let idle = server.idle_session_close();
    let every = (idle / 4).clamp(Duration::from_millis(50), Duration::from_mins(1));
    loop {
        tokio::select! {
            () = server.closing.cancelled() => return,
            () = tokio::time::sleep(every) => {}
        }
        for (session, profile) in server.hub().unattended(idle) {
            // Attributed to the session's profile, kept in `gateway.log`.
            let span = tracing::info_span!("reap", profile, session_id = session);
            close_idle(&server, &session, &profile, idle)
                .instrument(span)
                .await;
        }
        evict_beyond_cap(&server).await;
    }
}

/// Unloads the least recently attended detached sessions beyond
/// `gateway.limits.max_live_client_sessions` (spec: messaging-gateway,
/// Concurrency caps across surfaces); a session the host keeps (a running
/// turn, queued input) is skipped. Sessions the messaging gateway routes
/// stay attached to it, so only client sessions count.
pub(crate) async fn evict_beyond_cap(server: &Server) {
    let cap = server.max_live_client_sessions();
    if cap == 0 {
        return;
    }
    let detached = server.hub().detached();
    let mut excess = detached.len().saturating_sub(cap);
    for (session, profile) in detached {
        if excess == 0 {
            return;
        }
        match server.host().unload_idle(&profile, &session).await {
            Ok(true) => {
                tracing::info!(
                    profile,
                    session_id = session,
                    "unloaded a detached session over the cap"
                );
                server.hub().closed(&session);
                excess -= 1;
            }
            Ok(false) => {}
            Err(err) => tracing::warn!("cannot unload a detached session: {err}"),
        }
    }
}

async fn close_idle(server: &Server, session: &str, profile: &str, idle: Duration) {
    match server.host().close_idle(profile, session).await {
        Ok(true) => {
            tracing::info!("closed a session no client used for {idle:?}");
            server.hub().closed(session);
        }
        Ok(false) => {}
        Err(err) => tracing::warn!("cannot close an idle session: {err}"),
    }
}

/// Gives open connections a moment to send their 1001 close.
async fn wait_for_connections(server: &Server) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while server
        .connections
        .load(std::sync::atomic::Ordering::Relaxed)
        > 0
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
