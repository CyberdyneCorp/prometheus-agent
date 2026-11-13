//! The channel setup forms' actions (spec: web-dashboard, Channel setup
//! forms): Test and Restart adapter, through the daemon's messaging
//! gateway, and the interactive pairing flows an adapter declares
//! (`pmt_config::platforms::FirstParty::pairing`).
//!
//! A pairing flow runs in the daemon: `POST adapters/{name}/pairing`
//! starts it (409 while one runs for that adapter), every step it reports
//! (a refreshed QR image, a code) becomes the flow's current state, read
//! with `GET adapters/{name}/pairing`, and is streamed on `/api/events` as
//! `adapter.pairing`; `DELETE adapters/{name}/pairing` cancels it, after
//! which a late result is discarded and nothing is saved.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::platforms::{self, PairingFlow};
use pmt_core::cancel::CancelToken;
use pmt_gateway::messaging::actions::Tested;
use serde_json::{Value, json};

use crate::Shared;
use crate::host::PairingSink;
use crate::http::{Reply, error, from_error, json_reply};

/// The pairing flows of one listener, by (profile, platform).
#[derive(Debug, Default)]
pub(crate) struct Flows {
    flows: Mutex<HashMap<(String, String), Flow>>,
}

#[derive(Debug)]
struct Flow {
    /// The newest step, or the outcome once it ended.
    state: Value,
    cancel: CancelToken,
    running: bool,
}

impl Flows {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Flow>> {
        self.flows.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a step of a running flow (ignored once it ended).
    fn step(&self, key: &(String, String), state: &Value) -> bool {
        match self.lock().get_mut(key) {
            Some(flow) if flow.running && !flow.cancel.is_cancelled() => {
                flow.state = state.clone();
                true
            }
            _ => false,
        }
    }

    /// Records how a flow ended, unless it was cancelled first.
    fn end(&self, key: &(String, String), state: Value) -> Option<Value> {
        let mut flows = self.lock();
        let flow = flows.get_mut(key)?;
        flow.running = false;
        if flow.cancel.is_cancelled() {
            return None;
        }
        flow.state = state.clone();
        Some(state)
    }
}

fn gateway(shared: &Shared) -> Result<&pmt_gateway::messaging::Gateway, Reply> {
    shared
        .server
        .messaging()
        .ok_or_else(|| error(503, "messaging_off", "the messaging gateway is not running"))
}

/// `POST adapters/{name}/test`.
pub(crate) async fn test(shared: &Shared, profile: &str, name: &str) -> Reply {
    let gateway = match gateway(shared) {
        Ok(gateway) => gateway,
        Err(reply) => return reply,
    };
    match gateway.test_platform(profile, name).await {
        Ok(Tested::InboundOnly) => json_reply(
            200,
            &json!({"platform": name, "ok": true, "inbound_only": true}),
        ),
        Ok(Tested::Reached(took)) => json_reply(
            200,
            &json!({"platform": name, "ok": true,
                    "latency_ms": u64::try_from(took.as_millis()).unwrap_or(u64::MAX)}),
        ),
        Ok(Tested::Failed(why)) => {
            json_reply(200, &json!({"platform": name, "ok": false, "error": why}))
        }
        Err(err) => from_error(&err),
    }
}

/// `POST adapters/{name}/restart`.
pub(crate) async fn restart(shared: &Shared, profile: &str, name: &str) -> Reply {
    let gateway = match gateway(shared) {
        Ok(gateway) => gateway,
        Err(reply) => return reply,
    };
    match gateway.restart_platform(profile, name).await {
        Ok(status) => json_reply(
            200,
            &json!({"platform": name, "state": status.state, "message": status.message}),
        ),
        Err(err) => from_error(&err),
    }
}

/// The pairing flow `name` declares.
fn flow_of(name: &str) -> Result<PairingFlow, Reply> {
    platforms::first_party(name)
        .and_then(|p| p.pairing)
        .ok_or_else(|| {
            error(
                404,
                "not_found",
                &format!("the {name} adapter declares no pairing flow"),
            )
        })
}

/// `POST adapters/{name}/pairing`: starts the flow.
pub(crate) fn start(shared: &Arc<Shared>, profile: &str, name: &str) -> Reply {
    let flow = match flow_of(name) {
        Ok(flow) => flow,
        Err(reply) => return reply,
    };
    let key = (profile.to_owned(), name.to_owned());
    let cancel = CancelToken::new();
    {
        let mut flows = shared.pairings.lock();
        if flows.get(&key).is_some_and(|f| f.running) {
            return error(
                409,
                "pairing_running",
                "a pairing is already running for this adapter",
            );
        }
        flows.insert(
            key.clone(),
            Flow {
                state: json!({"state": "starting"}),
                cancel: cancel.clone(),
                running: true,
            },
        );
    }
    let stepped = Arc::clone(shared);
    let step_key = key.clone();
    let emit = Arc::new(move |state: Value| {
        if stepped.pairings.step(&step_key, &state) {
            stepped.feed.publish(
                &step_key.0,
                "adapter.pairing",
                with_platform(&step_key.1, state),
            );
        }
    });
    let sink = PairingSink::new(emit, cancel);
    let running = Arc::clone(shared);
    pmt_core::spawn::spawn(async move {
        let outcome = running.host.pair_adapter(&key.0, flow, sink).await;
        let state = match outcome {
            Ok(done) => done,
            Err(err) => json!({
                "state": if err.code().code == pmt_core::error::codes::GATEWAY_PAIRING_TIMEOUT.code { "expired" } else { "failed" },
                "message": err.message(),
            }),
        };
        if let Some(state) = running.pairings.end(&key, state) {
            running
                .feed
                .publish(&key.0, "adapter.pairing", with_platform(&key.1, state));
        }
    });
    json_reply(
        202,
        &json!({"platform": name, "flow": flow.as_str(), "state": "starting"}),
    )
}

fn with_platform(platform: &str, mut state: Value) -> Value {
    state["platform"] = platform.into();
    state
}

/// `GET adapters/{name}/pairing`: the flow's current state.
pub(crate) fn state(shared: &Shared, profile: &str, name: &str) -> Reply {
    if let Err(reply) = flow_of(name) {
        return reply;
    }
    let key = (profile.to_owned(), name.to_owned());
    let state = shared
        .pairings
        .lock()
        .get(&key)
        .map_or_else(|| json!({"state": "idle"}), |f| f.state.clone());
    json_reply(200, &with_platform(name, state))
}

/// `DELETE adapters/{name}/pairing`: cancels the flow; nothing is saved.
pub(crate) fn cancel(shared: &Shared, profile: &str, name: &str) -> Reply {
    if let Err(reply) = flow_of(name) {
        return reply;
    }
    let key = (profile.to_owned(), name.to_owned());
    let mut flows = shared.pairings.lock();
    let Some(flow) = flows.get_mut(&key).filter(|f| f.running) else {
        return error(404, "not_found", "no pairing is running for this adapter");
    };
    flow.cancel.cancel();
    flow.state = json!({"state": "cancelled"});
    drop(flows);
    shared.feed.publish(
        profile,
        "adapter.pairing",
        json!({"platform": name, "state": "cancelled"}),
    );
    json_reply(200, &json!({"platform": name, "state": "cancelled"}))
}
