//! The interactive pairing flows of one surface (spec: web-dashboard,
//! Channel setup forms; desktop-app, Telegram quick setup by QR), by
//! (profile, platform): at most one runs per adapter, every step it
//! reports becomes its current state, and once cancelled a late step or
//! result is discarded (the flow itself writes nothing after a cancel).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use pmt_core::cancel::CancelToken;
use pmt_core::error::Error;
use serde_json::{Value, json};

/// A flow's key: (profile, platform).
pub type FlowKey = (String, String);

#[derive(Debug)]
struct Flow {
    /// The newest step, or the outcome once it ended.
    state: Value,
    cancel: CancelToken,
    running: bool,
}

/// The pairing flows of one surface.
#[derive(Debug, Default)]
pub struct PairingFlows {
    flows: Mutex<HashMap<FlowKey, Flow>>,
}

impl PairingFlows {
    fn lock(&self) -> MutexGuard<'_, HashMap<FlowKey, Flow>> {
        self.flows.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts tracking a flow; `None` while one runs for that adapter.
    pub fn begin(&self, key: FlowKey) -> Option<CancelToken> {
        let mut flows = self.lock();
        if flows.get(&key).is_some_and(|f| f.running) {
            return None;
        }
        let cancel = CancelToken::new();
        flows.insert(
            key,
            Flow {
                state: json!({"state": "starting"}),
                cancel: cancel.clone(),
                running: true,
            },
        );
        Some(cancel)
    }

    /// Records a step of a running flow; false once it ended or was
    /// cancelled.
    pub fn step(&self, key: &FlowKey, state: &Value) -> bool {
        match self.lock().get_mut(key) {
            Some(flow) if flow.running && !flow.cancel.is_cancelled() => {
                flow.state = state.clone();
                true
            }
            _ => false,
        }
    }

    /// Records how a flow ended, unless it was cancelled first.
    pub fn end(&self, key: &FlowKey, state: Value) -> Option<Value> {
        let mut flows = self.lock();
        let flow = flows.get_mut(key)?;
        flow.running = false;
        if flow.cancel.is_cancelled() {
            return None;
        }
        flow.state = state.clone();
        Some(state)
    }

    /// The flow's current step or outcome (`idle` when none ran).
    #[must_use]
    pub fn state(&self, key: &FlowKey) -> Value {
        self.lock()
            .get(key)
            .map_or_else(|| json!({"state": "idle"}), |f| f.state.clone())
    }

    /// Cancels a running flow; false when none runs.
    pub fn cancel(&self, key: &FlowKey) -> bool {
        let mut flows = self.lock();
        let Some(flow) = flows.get_mut(key).filter(|f| f.running) else {
            return false;
        };
        flow.cancel.cancel();
        flow.state = json!({"state": "cancelled"});
        true
    }
}

/// The outcome a failed flow ends with: `expired` for a timeout,
/// `failed` otherwise, with the error's message.
#[must_use]
pub fn failed(err: &Error) -> Value {
    let expired = err.code().code == pmt_core::error::codes::GATEWAY_PAIRING_TIMEOUT.code;
    json!({
        "state": if expired { "expired" } else { "failed" },
        "message": err.message(),
    })
}
