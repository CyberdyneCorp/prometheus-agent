//! The daemon's per-session client state (spec: workspace-architecture,
//! Event sequencing and reconnect replay; Server-to-client requests;
//! Protocol handshake and versioning; tui, Server-to-client requests;
//! Server request deadlines fail closed; Orphaned attachments are reaped):
//! which connections are attached to which session and in what role, each
//! session's numbered event log kept for replay, the server-to-client
//! requests waiting for an answer, and since when a session has had no
//! client.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_rpc::Catalog;
use pmt_rpc::contracts::{
    AttachRole, Attached, Envelope, EventsSince, OpenRequest, RequestCancel, RequestResolved,
};
use pmt_rpc::frame::{self, Id};
use pmt_rpc::validate::Violation;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
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
    /// The client's id in `request.resolved` (`<kind>-<connection>`).
    pub client_id: String,
    /// Driver or observer.
    pub role: AttachRole,
}

impl Peer {
    /// Whether the session's questions go to this connection: a driver
    /// whose client answers server requests.
    fn answers(&self) -> bool {
        self.server_requests && self.role == AttachRole::Driver
    }
}

/// Where a re-attaching client left off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Since {
    /// The newest sequence number it applied.
    pub last_seen: u64,
    /// The epoch that number belongs to (`None`: this daemon's).
    pub epoch: Option<String>,
}

/// What became of an answer to a server request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answered {
    /// It settled the request.
    Taken,
    /// The request had already settled (answered, timed out, cancelled).
    Settled,
    /// No such request was sent to this connection, or the answer breaks
    /// the request's contract.
    Unknown,
}

/// How many settled request ids are remembered, so a late answer can be
/// told apart from an unknown id.
const SETTLED_KEPT: usize = 1024;

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
    /// When its last client left (`None` while one is attached, or once
    /// the host closed it).
    unattended_since: Option<Instant>,
}

#[derive(Debug)]
struct Pending {
    session_id: String,
    method: String,
    params: Value,
    reply: oneshot::Sender<(Value, String)>,
    sent_to: Vec<ConnId>,
}

#[derive(Debug, Default)]
struct State {
    sessions: HashMap<String, Log>,
    /// Every kept event, oldest first, for global FIFO eviction.
    order: VecDeque<(String, u64)>,
    total_bytes: usize,
    requests: HashMap<String, Pending>,
    /// Each session's interactive owner: the connection and its pid.
    owners: HashMap<String, (ConnId, Option<u32>)>,
    /// Recently settled request ids, oldest first.
    settled: VecDeque<String>,
    /// Recently taken answers (`id`, answer, answering client), oldest
    /// first, so a surface can show what another one decided.
    answers: VecDeque<(String, Value, String)>,
}

impl State {
    fn settle(&mut self, id: &str) {
        if self.settled.len() == SETTLED_KEPT {
            self.settled.pop_front();
        }
        self.settled.push_back(id.to_owned());
    }
}

/// Sessions, their event logs and the open server requests.
#[derive(Debug)]
pub struct Hub {
    epoch: String,
    catalog: Arc<Catalog>,
    limits: Limits,
    violations: Arc<Violations>,
    state: Mutex<State>,
    /// The sequence of daemon-wide events.
    daemon_seq: std::sync::atomic::AtomicU64,
}

/// One event to send: the frame and who may see it.
struct Delivery {
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
            daemon_seq: std::sync::atomic::AtomicU64::new(0),
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

    /// Attaches connection `conn` to a registered session. With `since`,
    /// the kept events after it are sent to `peer` first, unless some were
    /// evicted or `since` belongs to another epoch (`truncated`: the
    /// client reloads the transcript instead). Requests of the session
    /// still open are re-sent to a capable driver, which may answer them.
    pub fn attach(
        &self,
        conn: ConnId,
        session_id: &str,
        peer: Peer,
        since: Option<&Since>,
    ) -> Attached {
        let mut state = self.state();
        let log = state.sessions.entry(session_id.to_owned()).or_default();
        let truncated = since.is_some_and(|s| {
            s.epoch.as_deref().is_some_and(|e| e != self.epoch) || s.last_seen < log.evicted_through
        });
        if let Some(since) = since.filter(|_| !truncated) {
            for kept in log
                .events
                .iter()
                .filter(|k| k.envelope.seq > since.last_seen)
            {
                self.send_event(&peer, &kept.envelope);
            }
        }
        let latest_seq = log.next_seq;
        let answers = peer.answers();
        let tx = peer.tx.clone();
        log.attached.insert(conn, peer);
        log.unattended_since = None;
        // A connection that re-attaches as observer may no longer answer
        // what it was asked as driver (only drivers answer).
        let orphaned = if answers {
            Vec::new()
        } else {
            orphan_requests(&mut state, conn, Some(session_id))
        };
        let mut open_requests = Vec::new();
        for (id, pending) in &mut state.requests {
            if pending.session_id != session_id {
                continue;
            }
            if answers && !pending.sent_to.contains(&conn) {
                pending.sent_to.push(conn);
                let text = frame::request(&Id::Str(id.clone()), &pending.method, &pending.params);
                let _ = tx.send(Message::text(text));
            }
            open_requests.push(OpenRequest {
                id: id.clone(),
                method: pending.method.clone(),
                params: pending.params.clone(),
            });
        }
        drop(state);
        for (session, id) in orphaned {
            self.cancel_event(&session, &id, "no_responder");
        }
        Attached {
            session_id: session_id.to_owned(),
            latest_seq,
            epoch: self.epoch.clone(),
            truncated,
            open_requests,
        }
    }

    /// Whether `conn` is attached to `session_id`.
    #[must_use]
    pub fn is_attached(&self, conn: ConnId, session_id: &str) -> bool {
        self.role(conn, session_id).is_some()
    }

    /// The role `conn` attached to `session_id` with.
    #[must_use]
    pub fn role(&self, conn: ConnId, session_id: &str) -> Option<AttachRole> {
        self.state()
            .sessions
            .get(session_id)
            .and_then(|log| log.attached.get(&conn))
            .map(|peer| peer.role)
    }

    /// Sessions nobody has been attached to for at least `idle`, with no
    /// open request: candidates for the host to close.
    #[must_use]
    pub fn unattended(&self, idle: Duration) -> Vec<(String, String)> {
        let state = self.state();
        let now = Instant::now();
        state
            .sessions
            .iter()
            .filter(|(_, log)| {
                log.attached.is_empty()
                    && !log.profile.is_empty()
                    && log
                        .unattended_since
                        .is_some_and(|since| now.duration_since(since) >= idle)
            })
            .filter(|(id, _)| !state.requests.values().any(|p| &p.session_id == *id))
            .map(|(id, log)| (id.clone(), log.profile.clone()))
            .collect()
    }

    /// Sessions no client is attached to and that have no open request,
    /// least recently attended first: `(session, profile)`.
    pub fn detached(&self) -> Vec<(String, String)> {
        let state = self.state();
        let mut found: Vec<(Instant, String, String)> = state
            .sessions
            .iter()
            .filter(|(_, log)| log.attached.is_empty() && !log.profile.is_empty())
            .filter(|(id, _)| !state.requests.values().any(|p| &p.session_id == *id))
            .filter_map(|(id, log)| {
                log.unattended_since
                    .map(|since| (since, id.clone(), log.profile.clone()))
            })
            .collect();
        found.sort();
        found
            .into_iter()
            .map(|(_, id, profile)| (id, profile))
            .collect()
    }

    /// The host closed `session_id`: it is no longer a reaping candidate
    /// until a client attaches and leaves again.
    pub fn closed(&self, session_id: &str) {
        if let Some(log) = self.state().sessions.get_mut(session_id) {
            log.unattended_since = None;
        }
    }

    /// Makes `conn` (whose client reported `pid`) the interactive owner of
    /// `session_id`. Fails with the owner's pid while another connection
    /// owns it (spec: cli, Single interactive owner per session); the
    /// ownership ends when the owner detaches or disconnects.
    ///
    /// # Errors
    ///
    /// The current owner's pid (`None` when it reported none).
    pub fn claim(
        &self,
        conn: ConnId,
        session_id: &str,
        pid: Option<u32>,
    ) -> Result<(), Option<u32>> {
        let mut state = self.state();
        match state.owners.get(session_id) {
            Some((owner, owner_pid)) if *owner != conn => Err(*owner_pid),
            _ => {
                state.owners.insert(session_id.to_owned(), (conn, pid));
                Ok(())
            }
        }
    }

    /// Detaches `conn` from `session_id`; requests only it could answer
    /// resolve unanswered.
    pub fn detach(&self, conn: ConnId, session_id: &str) {
        let orphaned = {
            let mut state = self.state();
            if let Some(log) = state.sessions.get_mut(session_id) {
                leave(log, conn);
            }
            if state
                .owners
                .get(session_id)
                .is_some_and(|(o, _)| *o == conn)
            {
                state.owners.remove(session_id);
            }
            orphan_requests(&mut state, conn, Some(session_id))
        };
        for (session, id) in orphaned {
            self.cancel_event(&session, &id, "no_responder");
        }
    }

    /// Detaches `conn` from every session (it disconnected).
    pub fn detach_all(&self, conn: ConnId) {
        let orphaned = {
            let mut state = self.state();
            for log in state.sessions.values_mut() {
                leave(log, conn);
            }
            state.owners.retain(|_, (owner, _)| *owner != conn);
            orphan_requests(&mut state, conn, None)
        };
        for (session, id) in orphaned {
            self.cancel_event(&session, &id, "no_responder");
        }
    }

    /// Sends a daemon-level event (no session, not logged) to every
    /// connection attached to a session of `profile`, once each, except
    /// `except` (the sender's own connection).
    pub fn broadcast(&self, profile: &str, event: &str, data: Value, except: ConnId) {
        if let Err(violation) = self.catalog.check_outbound(event, &data, false) {
            self.violations.report(event, &violation);
            return;
        }
        let peers: Vec<Peer> = {
            let state = self.state();
            let mut seen = std::collections::HashSet::new();
            state
                .sessions
                .values()
                .filter(|log| log.profile == profile)
                .flat_map(|log| log.attached.iter())
                .filter(|(conn, _)| **conn != except && seen.insert(**conn))
                .map(|(_, peer)| peer.clone())
                .collect()
        };
        let envelope = Envelope {
            event: event.to_owned(),
            session_id: None,
            seq: self.daemon_seq.fetch_add(1, Ordering::Relaxed) + 1,
            epoch: self.epoch.clone(),
            data,
        };
        for peer in &peers {
            self.send_event(peer, &envelope);
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
            data,
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
        Delivery { envelope, peers }
    }

    fn deliver(&self, delivery: &Delivery) {
        for peer in &delivery.peers {
            self.send_event(peer, &delivery.envelope);
        }
    }

    /// Sends one event to `peer` when its protocol minor may see it, with
    /// the fields newer than that minor removed.
    fn send_event(&self, peer: &Peer, envelope: &Envelope) {
        let event = envelope.event.as_str();
        if !self
            .catalog
            .event_visible(event, peer.minor, &peer.advertised)
        {
            return;
        }
        let mut envelope = envelope.clone();
        self.catalog
            .strip_newer(event, &mut envelope.data, false, peer.minor);
        let params = serde_json::to_value(&envelope).unwrap_or_default();
        let _ = peer
            .tx
            .send(Message::text(frame::notification(event, &params)));
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

    /// Sends server request `method` to the session's capable drivers with
    /// its `deadline_ms`, and waits up to `timeout` for the first answer.
    /// `None` when no attached driver declared `server_requests` (at once,
    /// with one `request.cancel {reason: "no_responder"}`), on the deadline
    /// (`reason: "timeout"`), or when the asker gives up (the future is
    /// dropped, `reason: "cancelled"`). Every other recipient of an
    /// answered request gets `request.resolved` naming the answering
    /// client.
    pub async fn ask(
        self: &Arc<Self>,
        session_id: &str,
        method: &str,
        mut params: Value,
        timeout: Duration,
    ) -> Option<Value> {
        if let Some(fields) = params.as_object_mut() {
            let deadline = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
            fields.insert("deadline_ms".into(), deadline.into());
        }
        if let Err(violation) = self.catalog.check_outbound(method, &params, false) {
            self.violations.report(method, &violation);
            return None;
        }
        let Some((id, answer)) = self.open_request(session_id, method, params) else {
            let id = format!("srq-{}", random_hex(6));
            self.state().settle(&id);
            self.cancel_event(session_id, &id, "no_responder");
            return None;
        };
        let mut guard = CancelOnDrop {
            hub: Arc::clone(self),
            session_id: session_id.to_owned(),
            id: Some(id.clone()),
        };
        let outcome = tokio::time::timeout(timeout, answer).await;
        guard.id = None;
        match outcome {
            Ok(Ok((value, by_client))) => {
                self.publish_typed(
                    session_id,
                    "request.resolved",
                    &RequestResolved { id, by_client },
                );
                Some(value)
            }
            Ok(Err(_)) => None,
            Err(_) => {
                let removed = {
                    let mut state = self.state();
                    state.settle(&id);
                    state.requests.remove(&id).is_some()
                };
                if removed {
                    self.cancel_event(session_id, &id, "timeout");
                }
                None
            }
        }
    }

    fn open_request(
        &self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Option<(String, oneshot::Receiver<(Value, String)>)> {
        let mut state = self.state();
        let capable: Vec<(ConnId, Peer)> = state
            .sessions
            .get(session_id)?
            .attached
            .iter()
            .filter(|(_, p)| p.answers())
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

    /// An answer from client `by_client` on connection `conn` to request
    /// `id`. Only the first valid answer of a recipient settles it; an
    /// error answer settles it unanswered.
    pub fn resolve(
        &self,
        conn: ConnId,
        by_client: &str,
        id: &str,
        answer: Result<Value, pmt_rpc::RpcError>,
    ) -> Answered {
        let mut state = self.state();
        let Some(pending) = state.requests.get(id) else {
            return if state.settled.iter().any(|s| s == id) {
                Answered::Settled
            } else {
                Answered::Unknown
            };
        };
        if !pending.sent_to.contains(&conn) {
            return Answered::Unknown;
        }
        if let Ok(value) = &answer
            && let Err(violation) = self.catalog.check_answer(&pending.method, value)
        {
            tracing::warn!(
                "dropped an invalid answer to {}: {violation}",
                pending.method
            );
            return Answered::Unknown;
        }
        let Some(pending) = state.requests.remove(id) else {
            return Answered::Unknown;
        };
        state.settle(id);
        if let Ok(value) = answer {
            if state.answers.len() == SETTLED_KEPT {
                state.answers.pop_front();
            }
            state
                .answers
                .push_back((id.to_owned(), value.clone(), by_client.to_owned()));
            let _ = pending.reply.send((value, by_client.to_owned()));
        }
        Answered::Taken
    }

    /// The answer that settled request `id` and the client that gave it,
    /// while it is among the recently settled ones.
    #[must_use]
    pub fn answer_of(&self, id: &str) -> Option<(Value, String)> {
        self.state()
            .answers
            .iter()
            .find(|(settled, _, _)| settled == id)
            .map(|(_, value, by)| (value.clone(), by.clone()))
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

/// Removes `conn` from the session's clients; the last one leaving starts
/// the session's unattended time.
fn leave(log: &mut Log, conn: ConnId) {
    if log.attached.remove(&conn).is_some() && log.attached.is_empty() {
        log.unattended_since = Some(Instant::now());
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

/// Removes `conn` from the open requests of `session` (of every session
/// with `None`); the requests nobody else can answer are removed (their
/// asker sees no answer) and returned.
fn orphan_requests(
    state: &mut State,
    conn: ConnId,
    session: Option<&str>,
) -> Vec<(String, String)> {
    let mut orphaned = Vec::new();
    for (id, pending) in &mut state.requests {
        if session.is_some_and(|s| s != pending.session_id) {
            continue;
        }
        pending.sent_to.retain(|c| *c != conn);
        if pending.sent_to.is_empty() {
            orphaned.push((pending.session_id.clone(), id.clone()));
        }
    }
    for (_, id) in &orphaned {
        state.requests.remove(id);
        state.settle(id);
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
            let removed = {
                let mut state = self.hub.state();
                state.settle(&id);
                state.requests.remove(&id).is_some()
            };
            if removed {
                self.hub.cancel_event(&self.session_id, &id, "cancelled");
            }
        }
    }
}

#[cfg(test)]
mod tests;
