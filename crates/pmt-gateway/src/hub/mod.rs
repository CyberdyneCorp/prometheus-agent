//! The daemon's per-session client state (spec: workspace-architecture,
//! Event sequencing and reconnect replay; Server-to-client requests;
//! Protocol handshake and versioning): which connections are attached to
//! which session, each session's numbered event log kept for replay, and
//! the server-to-client requests waiting for an answer.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_rpc::Catalog;
use pmt_rpc::contracts::{Envelope, EventsSince, OpenRequest, RequestCancel, RequestResolved};
use pmt_rpc::frame::{self, Id};
use pmt_rpc::validate::Violation;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::tickets::random_hex;

/// A connection's number within the daemon.
pub type ConnId = u64;

/// How the hub reaches one attached connection.
#[derive(Debug, Clone)]
pub struct Peer {
    /// The connection's outbound frames.
    pub tx: mpsc::UnboundedSender<Message>,
    /// The protocol minor the client speaks.
    pub minor: u16,
    /// Newer events it advertised.
    pub advertised: Arc<Vec<String>>,
    /// It answers server-to-client requests.
    pub server_requests: bool,
}

/// What the event logs may hold.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Events kept per session.
    pub session_events: usize,
    /// Bytes kept per session.
    pub session_bytes: usize,
    /// Sessions with kept events.
    pub sessions: usize,
    /// Bytes kept in total.
    pub total_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            session_events: 512,
            session_bytes: 4 * 1024 * 1024,
            sessions: 64,
            total_bytes: 64 * 1024 * 1024,
        }
    }
}

/// How a contract violation by the daemon itself is handled: debug and
/// test builds abort, release builds log at ERROR and count it in
/// `prometheus.rpc.contract_violations` (spec: workspace-architecture,
/// Strict parameter validation and contract violations).
#[derive(Debug)]
pub struct Violations {
    abort: bool,
    count: AtomicU64,
}

/// The metric counting contract violations.
pub const VIOLATIONS_METRIC: &str = "prometheus.rpc.contract_violations";

impl Violations {
    /// The policy of this build.
    #[must_use]
    pub fn for_build() -> Self {
        Self::new(cfg!(debug_assertions))
    }

    /// `abort = false` logs and counts (the release policy).
    #[must_use]
    pub fn new(abort: bool) -> Self {
        Self {
            abort,
            count: AtomicU64::new(0),
        }
    }

    /// Records a violation of `name`'s contract.
    pub fn report(&self, name: &str, violation: &Violation) {
        tracing::error!(
            metric = VIOLATIONS_METRIC,
            "RPC contract violation by the daemon in {name}: {violation}"
        );
        self.count.fetch_add(1, Ordering::Relaxed);
        if self.abort {
            eprintln!("RPC contract violation by the daemon in {name}: {violation}");
            std::process::abort();
        }
    }

    /// Violations so far.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
struct Kept {
    envelope: Envelope,
    bytes: usize,
}

#[derive(Debug, Default)]
struct Log {
    profile: String,
    next_seq: u64,
    events: VecDeque<Kept>,
    bytes: usize,
    evicted_through: u64,
    attached: HashMap<ConnId, Peer>,
}

#[derive(Debug)]
struct Pending {
    session_id: String,
    method: String,
    params: Value,
    reply: oneshot::Sender<Value>,
    sent_to: Vec<ConnId>,
}

#[derive(Debug, Default)]
struct State {
    sessions: HashMap<String, Log>,
    /// Every kept event, oldest first, for global FIFO eviction.
    order: VecDeque<(String, u64)>,
    total_bytes: usize,
    requests: HashMap<String, Pending>,
}

/// Sessions, their event logs and the open server requests.
#[derive(Debug)]
pub struct Hub {
    epoch: String,
    catalog: Arc<Catalog>,
    limits: Limits,
    violations: Arc<Violations>,
    state: Mutex<State>,
}

/// One event to send: the frame and who may see it.
struct Delivery {
    event: String,
    data: Value,
    envelope: Envelope,
    peers: Vec<Peer>,
}

impl Hub {
    /// An empty hub for a daemon whose start drew `epoch`.
    #[must_use]
    pub fn new(
        epoch: String,
        catalog: Arc<Catalog>,
        limits: Limits,
        violations: Arc<Violations>,
    ) -> Self {
        Self {
            epoch,
            catalog,
            limits,
            violations,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("hub state poisoned")
    }

    /// The daemon epoch.
    #[must_use]
    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// Records that `session_id` belongs to `profile`.
    pub fn register(&self, session_id: &str, profile: &str) {
        let mut state = self.state();
        let log = state.sessions.entry(session_id.to_owned()).or_default();
        profile.clone_into(&mut log.profile);
    }

    /// The profile of a known session.
    #[must_use]
    pub fn profile_of(&self, session_id: &str) -> Option<String> {
        let state = self.state();
        state
            .sessions
            .get(session_id)
            .filter(|log| !log.profile.is_empty())
            .map(|log| log.profile.clone())
    }

    /// Attaches connection `conn` to a registered session; returns its
    /// newest sequence number.
    pub fn attach(&self, conn: ConnId, session_id: &str, peer: Peer) -> u64 {
        let mut state = self.state();
        let log = state.sessions.entry(session_id.to_owned()).or_default();
        log.attached.insert(conn, peer);
        log.next_seq
    }

    /// Whether `conn` is attached to `session_id`.
    #[must_use]
    pub fn is_attached(&self, conn: ConnId, session_id: &str) -> bool {
        self.state()
            .sessions
            .get(session_id)
            .is_some_and(|log| log.attached.contains_key(&conn))
    }

    /// Detaches `conn` from `session_id`; requests only it could answer
    /// resolve unanswered.
    pub fn detach(&self, conn: ConnId, session_id: &str) {
        let orphaned = {
            let mut state = self.state();
            if let Some(log) = state.sessions.get_mut(session_id) {
                log.attached.remove(&conn);
            }
            orphan_requests(&mut state, conn)
        };
        for (session, id) in orphaned {
            self.cancel_event(&session, &id, "detached");
        }
    }

    /// Detaches `conn` from every session (it disconnected).
    pub fn detach_all(&self, conn: ConnId) {
        let orphaned = {
            let mut state = self.state();
            for log in state.sessions.values_mut() {
                log.attached.remove(&conn);
            }
            orphan_requests(&mut state, conn)
        };
        for (session, id) in orphaned {
            self.cancel_event(&session, &id, "detached");
        }
    }

    /// Appends an event to the session's log and sends it to every
    /// attached connection that may see it.
    pub fn publish(&self, session_id: &str, event: &str, data: Value) {
        if let Err(violation) = self.catalog.check_outbound(event, &data, false) {
            self.violations.report(event, &violation);
            return;
        }
        let delivery = self.record(session_id, event, data);
        self.deliver(&delivery);
    }

    fn record(&self, session_id: &str, event: &str, data: Value) -> Delivery {
        let mut state = self.state();
        let log = state.sessions.entry(session_id.to_owned()).or_default();
        log.next_seq += 1;
        let envelope = Envelope {
            event: event.to_owned(),
            session_id: Some(session_id.to_owned()),
            seq: log.next_seq,
            epoch: self.epoch.clone(),
            data: data.clone(),
        };
        let bytes = serde_json::to_string(&envelope).map_or(0, |s| s.len());
        log.events.push_back(Kept {
            envelope: envelope.clone(),
            bytes,
        });
        log.bytes += bytes;
        let peers = log.attached.values().cloned().collect();
        let seq = envelope.seq;
        state.total_bytes += bytes;
        state.order.push_back((session_id.to_owned(), seq));
        self.enforce(&mut state, session_id);
        Delivery {
            event: event.to_owned(),
            data,
            envelope,
            peers,
        }
    }

    fn deliver(&self, delivery: &Delivery) {
        for peer in &delivery.peers {
            if !self
                .catalog
                .event_visible(&delivery.event, peer.minor, &peer.advertised)
            {
                continue;
            }
            let mut envelope = delivery.envelope.clone();
            let mut data = delivery.data.clone();
            self.catalog
                .strip_newer(&delivery.event, &mut data, false, peer.minor);
            envelope.data = data;
            let params = serde_json::to_value(&envelope).unwrap_or_default();
            let _ = peer
                .tx
                .send(Message::text(frame::notification(&delivery.event, &params)));
        }
    }

    /// Evicts the oldest events until every limit holds.
    fn enforce(&self, state: &mut State, session_id: &str) {
        let limits = self.limits;
        let State {
            sessions,
            total_bytes,
            ..
        } = state;
        if let Some(log) = sessions.get_mut(session_id) {
            while log.events.len() > limits.session_events || log.bytes > limits.session_bytes {
                *total_bytes -= evict_front(log);
            }
        }
        loop {
            let busy = state
                .sessions
                .values()
                .filter(|l| !l.events.is_empty())
                .count();
            if busy <= limits.sessions && state.total_bytes <= limits.total_bytes {
                break;
            }
            let Some((session, seq)) = state.order.pop_front() else {
                break;
            };
            let Some(log) = state.sessions.get_mut(&session) else {
                continue;
            };
            if log.events.front().is_some_and(|k| k.envelope.seq == seq) {
                state.total_bytes -= evict_front(log);
            }
        }
    }

    /// `session.events.since`.
    #[must_use]
    pub fn events_since(&self, session_id: &str, last_seen: u64) -> EventsSince {
        let state = self.state();
        let log = state.sessions.get(session_id);
        let events = log
            .map(|log| {
                log.events
                    .iter()
                    .filter(|k| k.envelope.seq > last_seen)
                    .map(|k| k.envelope.clone())
                    .collect()
            })
            .unwrap_or_default();
        let open_requests = state
            .requests
            .iter()
            .filter(|(_, p)| p.session_id == session_id)
            .map(|(id, p)| OpenRequest {
                id: id.clone(),
                method: p.method.clone(),
                params: p.params.clone(),
            })
            .collect();
        EventsSince {
            events,
            latest_seq: log.map_or(0, |l| l.next_seq),
            truncated: log.is_some_and(|l| last_seen < l.evicted_through),
            epoch: self.epoch.clone(),
            open_requests,
        }
    }

    /// Sends server request `method` to the session's capable connections
    /// and waits up to `timeout` for the first answer. `None` when no
    /// attached connection declared `server_requests` (at once), on the
    /// deadline, or when the asker gives up (the future is dropped); the
    /// last two emit one `request.cancel`.
    pub async fn ask(
        self: &Arc<Self>,
        session_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Option<Value> {
        if let Err(violation) = self.catalog.check_outbound(method, &params, false) {
            self.violations.report(method, &violation);
            return None;
        }
        let (id, answer) = self.open_request(session_id, method, params)?;
        let mut guard = CancelOnDrop {
            hub: Arc::clone(self),
            session_id: session_id.to_owned(),
            id: Some(id.clone()),
        };
        let outcome = tokio::time::timeout(timeout, answer).await;
        guard.id = None;
        match outcome {
            Ok(Ok(value)) => {
                self.publish_typed(session_id, "request.resolved", &RequestResolved { id });
                Some(value)
            }
            Ok(Err(_)) => None,
            Err(_) => {
                self.state().requests.remove(&id);
                self.cancel_event(session_id, &id, "timeout");
                None
            }
        }
    }

    fn open_request(
        &self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Option<(String, oneshot::Receiver<Value>)> {
        let mut state = self.state();
        let capable: Vec<(ConnId, Peer)> = state
            .sessions
            .get(session_id)?
            .attached
            .iter()
            .filter(|(_, p)| p.server_requests)
            .map(|(c, p)| (*c, p.clone()))
            .collect();
        if capable.is_empty() {
            return None;
        }
        let id = format!("srq-{}", random_hex(6));
        let text = frame::request(&Id::Str(id.clone()), method, &params);
        for (_, peer) in &capable {
            let _ = peer.tx.send(Message::text(text.clone()));
        }
        let (reply, answer) = oneshot::channel();
        state.requests.insert(
            id.clone(),
            Pending {
                session_id: session_id.to_owned(),
                method: method.to_owned(),
                params,
                reply,
                sent_to: capable.into_iter().map(|(c, _)| c).collect(),
            },
        );
        Some((id, answer))
    }

    /// An answer from connection `conn` to request `id`; `false` when it
    /// was dropped (closed id, another connection's request, or an answer
    /// that breaks the contract).
    pub fn resolve(
        &self,
        conn: ConnId,
        id: &str,
        answer: Result<Value, pmt_rpc::RpcError>,
    ) -> bool {
        let mut state = self.state();
        let Some(pending) = state.requests.get(id) else {
            return false;
        };
        if !pending.sent_to.contains(&conn) {
            return false;
        }
        if let Ok(value) = &answer
            && let Err(violation) = self.catalog.check_answer(&pending.method, value)
        {
            tracing::warn!(
                "dropped an invalid answer to {}: {violation}",
                pending.method
            );
            return false;
        }
        let Some(pending) = state.requests.remove(id) else {
            return false;
        };
        if let Ok(value) = answer {
            let _ = pending.reply.send(value);
        }
        true
    }

    /// The session of an open request.
    #[must_use]
    pub fn request_session(&self, id: &str) -> Option<String> {
        self.state().requests.get(id).map(|p| p.session_id.clone())
    }

    fn cancel_event(&self, session_id: &str, id: &str, reason: &str) {
        let payload = RequestCancel {
            id: id.to_owned(),
            reason: reason.to_owned(),
        };
        self.publish_typed(session_id, "request.cancel", &payload);
    }

    fn publish_typed(&self, session_id: &str, event: &str, payload: &impl serde::Serialize) {
        self.publish(
            session_id,
            event,
            serde_json::to_value(payload).unwrap_or_default(),
        );
    }

    /// Sessions with state in the hub.
    #[must_use]
    pub fn sessions(&self) -> usize {
        self.state().sessions.len()
    }
}

fn evict_front(log: &mut Log) -> usize {
    match log.events.pop_front() {
        Some(kept) => {
            log.bytes -= kept.bytes;
            log.evicted_through = kept.envelope.seq;
            kept.bytes
        }
        None => 0,
    }
}

/// Removes `conn` from every open request; the requests nobody else can
/// answer are removed (their asker sees no answer) and returned.
fn orphan_requests(state: &mut State, conn: ConnId) -> Vec<(String, String)> {
    let mut orphaned = Vec::new();
    for (id, pending) in &mut state.requests {
        pending.sent_to.retain(|c| *c != conn);
        if pending.sent_to.is_empty() {
            orphaned.push((pending.session_id.clone(), id.clone()));
        }
    }
    for (_, id) in &orphaned {
        state.requests.remove(id);
    }
    orphaned
}

struct CancelOnDrop {
    hub: Arc<Hub>,
    session_id: String,
    id: Option<String>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let removed = self.hub.state().requests.remove(&id).is_some();
            if removed {
                self.hub.cancel_event(&self.session_id, &id, "cancelled");
            }
        }
    }
}

#[cfg(test)]
mod tests;
