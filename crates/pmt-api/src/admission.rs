//! Idempotent admission (spec: api-server, Idempotent admission for Chat
//! Completions and Responses; Run idempotency ledger). An
//! `Idempotency-Key` is reserved in the profile's store, scoped to the
//! endpoint and the presented bearer key, before any work starts. An
//! exact retry gets the original answer (waiting for it while the first
//! request still runs here); the same key for other work is refused; a key
//! whose work left no answer (the daemon stopped mid-turn) is refused as
//! `unknown_execution` rather than run twice.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pmt_store::Store;
use pmt_store::api::admissions::{Endpoint, Key, Reserved};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::sync::watch;

use crate::auth::Caller;
use crate::http::{Reply, bad_request, conflict, from_error};

/// The key header.
pub(crate) const HEADER: &str = "idempotency-key";

/// The request's `Idempotency-Key`: 1-255 visible ASCII characters.
pub(crate) fn key_of(req: &crate::http::Req) -> Result<Option<String>, Reply> {
    let Some(raw) = req.headers.get(HEADER) else {
        return Ok(None);
    };
    let bytes = raw.as_bytes();
    if bytes.is_empty() || bytes.len() > 255 || !bytes.iter().all(|b| (0x21..=0x7e).contains(b)) {
        return Err(bad_request(
            "invalid_idempotency_key",
            "Idempotency-Key must be 1-255 visible ASCII characters",
        ));
    }
    Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
}

/// The hash of a request's identity fields.
pub(crate) fn identity(fields: &Value) -> String {
    let digest = Sha256::digest(fields.to_string().as_bytes());
    pmt_gateway::messaging::sign::hex(&digest)
}

/// What admission decided.
#[derive(Debug)]
pub(crate) enum Admitted {
    /// No key: run the work.
    Unkeyed,
    /// A fresh reservation: run the work, then settle or release it.
    Fresh(Ticket),
    /// An exact retry of finished work: its answer.
    Replay(String),
}

/// A fresh reservation; while it lives, retries of its key wait for it.
#[derive(Debug)]
pub(crate) struct Ticket {
    pub id: String,
    _running: Running,
}

#[derive(Debug)]
struct Running {
    id: String,
    done: watch::Sender<bool>,
    inflight: Arc<Mutex<HashMap<String, watch::Receiver<bool>>>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.inflight
            .lock()
            .expect("inflight poisoned")
            .remove(&self.id);
        let _ = self.done.send(true);
    }
}

/// Reservations whose work runs in this process.
#[derive(Debug, Default)]
pub(crate) struct InFlight {
    running: Arc<Mutex<HashMap<String, watch::Receiver<bool>>>>,
}

impl InFlight {
    fn start(&self, id: &str) -> Running {
        let (done, rx) = watch::channel(false);
        self.running
            .lock()
            .expect("inflight poisoned")
            .insert(id.to_owned(), rx);
        Running {
            id: id.to_owned(),
            done,
            inflight: Arc::clone(&self.running),
        }
    }

    fn waiter(&self, id: &str) -> Option<watch::Receiver<bool>> {
        self.running
            .lock()
            .expect("inflight poisoned")
            .get(id)
            .cloned()
    }
}

/// The request being admitted.
pub(crate) struct Request<'a> {
    pub endpoint: Endpoint,
    pub key: Option<&'a str>,
    pub identity: &'a str,
    pub session: Option<&'a str>,
    /// The code of a key reused for other work.
    pub conflict_code: &'a str,
}

/// Admits `request` of `caller`.
pub(crate) async fn admit(
    inflight: &InFlight,
    store: &Arc<Store>,
    caller: &Caller,
    request: &Request<'_>,
) -> Result<Admitted, Reply> {
    let Some(idem_key) = request.key else {
        return Ok(Admitted::Unkeyed);
    };
    let key = Key {
        endpoint: request.endpoint,
        key_fp: &caller.key_fp,
        idem_key,
    };
    let reserved = store
        .reserve_admission(key, request.identity, request.session)
        .map_err(|e| from_error(&e))?;
    let existing = match reserved {
        Reserved::Fresh(admission) => {
            let running = inflight.start(&admission.id);
            return Ok(Admitted::Fresh(Ticket {
                id: admission.id,
                _running: running,
            }));
        }
        Reserved::Existing(existing) => existing,
    };
    if existing.identity != request.identity {
        return Err(conflict(
            request.conflict_code,
            "this Idempotency-Key was used for a different request",
        ));
    }
    if existing.tombstone_until.is_some() {
        return Err(unknown("the session this key ran in was deleted"));
    }
    if let Some(answer) = existing.answer {
        return Ok(Admitted::Replay(answer));
    }
    if let Some(mut done) = inflight.waiter(&existing.id) {
        let _ = done.wait_for(|finished| *finished).await;
        let reserved = store
            .reserve_admission(key, request.identity, request.session)
            .map_err(|e| from_error(&e))?;
        if let Reserved::Existing(settled) = reserved
            && let Some(answer) = settled.answer
        {
            return Ok(Admitted::Replay(answer));
        }
    }
    Err(unknown(
        "the request with this Idempotency-Key started but left no answer to replay",
    ))
}

fn unknown(message: &str) -> Reply {
    conflict("unknown_execution", message)
}

/// What admission let through.
pub(crate) enum Gate {
    /// An exact retry of finished work: its answer.
    Replay(String),
    /// Work to run, holding its reservation and run slot.
    Run(Finish),
}

/// Admits `request` and takes one of the profile's `max` run slots; a
/// fresh reservation is dropped again when no slot is free, so the exact
/// retry stays safe.
pub(crate) async fn admit_run(
    shared: &crate::Shared,
    store: &Arc<Store>,
    caller: &Caller,
    request: &Request<'_>,
    max: u32,
) -> Result<Gate, Reply> {
    let ticket = match admit(&shared.inflight, store, caller, request).await? {
        Admitted::Replay(answer) => return Ok(Gate::Replay(answer)),
        Admitted::Fresh(ticket) => Some(ticket),
        Admitted::Unkeyed => None,
    };
    match shared.limits.acquire(&caller.profile, max) {
        Ok(permit) => Ok(Gate::Run(Finish {
            store: Arc::clone(store),
            ticket,
            _permit: permit,
        })),
        Err(refused) => {
            if let Some(ticket) = &ticket {
                release(store, ticket);
            }
            Err(refused)
        }
    }
}

/// What happens once admitted work is over: its answer is recorded for
/// retries (or, never admitted after all, its reservation dropped), and
/// its run slot is freed.
pub(crate) struct Finish {
    pub store: Arc<Store>,
    pub ticket: Option<Ticket>,
    pub _permit: crate::limits::Permit,
}

impl Finish {
    /// Records `answer` for exact retries.
    pub(crate) fn settle(&self, session: &str, answer: &Value) {
        if let Some(ticket) = &self.ticket {
            settle(&self.store, ticket, Some(session), &answer.to_string());
        }
    }

    /// The work was never admitted: an exact retry may run it.
    pub(crate) fn release(&self) {
        if let Some(ticket) = &self.ticket {
            release(&self.store, ticket);
        }
    }
}

/// Records the answer of a fresh reservation.
pub(crate) fn settle(store: &Store, ticket: &Ticket, session: Option<&str>, answer: &str) {
    if let Err(err) = store.settle_admission(&ticket.id, session, answer) {
        tracing::warn!("idempotency answer not recorded: {err}");
    }
}

/// Drops a fresh reservation whose work was never admitted.
pub(crate) fn release(store: &Store, ticket: &Ticket) {
    if let Err(err) = store.release_admission(&ticket.id) {
        tracing::warn!("idempotency reservation not released: {err}");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::identity;

    #[test]
    fn identity_follows_the_fields() {
        assert_eq!(identity(&json!({"a": 1})), identity(&json!({"a": 1})));
        assert_ne!(identity(&json!({"a": 1})), identity(&json!({"a": 2})));
    }
}
