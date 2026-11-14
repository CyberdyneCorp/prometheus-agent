//! The API server's hub connection: like the messaging gateway it attaches
//! to the sessions it runs turns in, as a driver that answers server
//! requests, and hands each session's events and requests to the turn
//! that watches it. Approval and clarify answers go back through the hub,
//! so they settle the same pending request every attached client sees.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pmt_gateway::Server;
use pmt_gateway::hub::{Answered, ConnId, Peer as HubPeer};
use pmt_rpc::contracts::{AttachRole, TurnFinished};
use pmt_rpc::frame::{self, Id, Incoming};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// What a watched session said.
#[derive(Debug, Clone)]
pub(crate) enum PeerMsg {
    /// A `session.event`: its type and payload.
    Event { kind: String, payload: Value },
    /// `turn.finished`.
    Finished(TurnFinished),
    /// A server request (`approval.request`, `clarify.request`, ...).
    Asked {
        id: String,
        method: String,
        params: Value,
    },
    /// A request settled elsewhere (answered by another client, timed
    /// out or cancelled).
    Settled { id: String },
}

/// The hub connection.
pub(crate) struct Peer {
    server: Arc<Server>,
    conn: ConnId,
    attached_as: HubPeer,
    routes: Mutex<HashMap<String, mpsc::UnboundedSender<PeerMsg>>>,
}

impl Peer {
    /// Opens the connection and starts reading what the hub sends it.
    pub(crate) fn start(server: &Arc<Server>) -> Arc<Self> {
        let conn = server.next_conn_id();
        let (tx, rx) = mpsc::unbounded_channel();
        let peer = Arc::new(Self {
            server: Arc::clone(server),
            conn,
            attached_as: HubPeer {
                tx,
                minor: server.protocol().minor,
                advertised: Arc::default(),
                server_requests: true,
                features: Arc::default(),
                client_id: format!("api-{conn}"),
                role: AttachRole::Driver,
            },
            routes: Mutex::default(),
        });
        pmt_core::spawn::spawn(read(Arc::downgrade(&peer), rx));
        peer
    }

    /// Attaches to `session` of `profile`; its traffic goes to the
    /// returned receiver until [`Self::unwatch`].
    pub(crate) fn watch(&self, profile: &str, session: &str) -> mpsc::UnboundedReceiver<PeerMsg> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes
            .lock()
            .expect("routes poisoned")
            .insert(session.to_owned(), tx);
        let hub = self.server.hub();
        hub.register(session, profile);
        if !hub.is_attached(self.conn, session) {
            hub.attach(self.conn, session, self.attached_as.clone(), None);
        }
        rx
    }

    /// Stops watching `session` and detaches from it.
    pub(crate) fn unwatch(&self, session: &str) {
        self.routes.lock().expect("routes poisoned").remove(session);
        self.server.hub().detach(self.conn, session);
    }

    /// Answers server request `id`; `None` settles it unanswered.
    pub(crate) fn answer(&self, id: &str, answer: Option<Value>) -> Answered {
        let answer = answer.ok_or_else(|| {
            pmt_rpc::RpcError::from_error(&pmt_core::Error::new(
                &pmt_core::error::codes::RPC_REQUEST_CLOSED,
                "the API request has no answer channel",
            ))
        });
        self.server
            .hub()
            .resolve(self.conn, &self.attached_as.client_id, id, answer)
    }

    fn route(&self, session: &str, msg: PeerMsg) {
        if let Some(tx) = self.routes.lock().expect("routes poisoned").get(session) {
            let _ = tx.send(msg);
        }
    }
}

async fn read(peer: std::sync::Weak<Peer>, mut rx: mpsc::UnboundedReceiver<Message>) {
    while let Some(message) = rx.recv().await {
        let Some(peer) = peer.upgrade() else {
            return;
        };
        let Message::Text(text) = message else {
            continue;
        };
        match frame::parse(text.as_str()) {
            Ok(Incoming::Notification { method, params }) => notification(&peer, &method, &params),
            Ok(Incoming::Request {
                id: Id::Str(id),
                method,
                params,
                ..
            }) => {
                if let Some(session) = params["session_id"].as_str().map(str::to_owned) {
                    peer.route(&session, PeerMsg::Asked { id, method, params });
                }
            }
            _ => {}
        }
    }
}

fn notification(peer: &Peer, method: &str, envelope: &Value) {
    let Some(session) = envelope["session_id"].as_str() else {
        return;
    };
    let data = &envelope["data"];
    let msg = match method {
        "session.event" => PeerMsg::Event {
            kind: data["type"].as_str().unwrap_or_default().to_owned(),
            payload: data["payload"].clone(),
        },
        "turn.finished" => match serde_json::from_value(data.clone()) {
            Ok(finished) => PeerMsg::Finished(finished),
            Err(_) => return,
        },
        "request.resolved" | "request.cancel" => PeerMsg::Settled {
            id: data["id"].as_str().unwrap_or_default().to_owned(),
        },
        _ => return,
    };
    peer.route(session, msg);
}
