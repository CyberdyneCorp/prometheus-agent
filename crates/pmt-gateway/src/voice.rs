//! Voice mode's daemon methods (spec: voice-mode, Wake capture location
//! for remote clients; Client-direct voice for remote daemons):
//! `wake.arm`, `wake.feed`, `wake.disarm`, `wake.status`,
//! `voice.client_config` and `voice.transcribe` for the profile the call
//! names (the connection's grant must cover it). The host runs them; a listener armed by a connection
//! reports `wake.detected` to that connection, and its owner is gone when
//! the connection is.

use std::sync::{Arc, Weak};

use pmt_rpc::Catalog;
use pmt_rpc::contracts::{self as c, AttachRole, Method};
use serde_json::Value;

use crate::methods::{profile_for, rpc};
use crate::router::Router;
use crate::server::{Call, Conn, Server};

/// The connection that armed a wake listener: where `wake.detected`
/// goes.
#[derive(Clone)]
pub struct Caller {
    server: Weak<Server>,
    conn: Weak<Conn>,
}

impl std::fmt::Debug for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Caller")
            .field("connected", &self.connected())
            .finish()
    }
}

impl Caller {
    /// Whether the connection is still open.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.conn.strong_count() > 0
    }

    /// Sends `wake.detected` to the connection.
    pub fn detected(&self, detected: &c::WakeDetected) {
        let (Some(server), Some(conn)) = (self.server.upgrade(), self.conn.upgrade()) else {
            return;
        };
        let data = serde_json::to_value(detected).unwrap_or(Value::Null);
        server
            .hub()
            .notify(&[conn.peer(AttachRole::Observer)], "wake.detected", data);
    }

    /// A client that is never connected (tests, the daemon itself).
    #[must_use]
    pub fn none() -> Self {
        Self {
            server: Weak::new(),
            conn: Weak::new(),
        }
    }
}

/// One voice call: the method name, its params and the calling
/// connection.
#[derive(Debug, Clone)]
pub struct VoiceCall {
    /// The method name.
    pub method: &'static str,
    /// Its params (as the contract parsed them).
    pub params: Value,
    /// The calling connection.
    pub client: Caller,
}

/// Registers the methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    add::<c::WakeArm>(router, catalog)?;
    add::<c::WakeFeed>(router, catalog)?;
    add::<c::WakeDisarm>(router, catalog)?;
    add::<c::WakeStatus>(router, catalog)?;
    add::<c::VoiceClientConfigGet>(router, catalog)?;
    add::<c::VoiceTranscribe>(router, catalog)?;
    Ok(())
}

fn add<M: Method>(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router.register::<M, _, _>(catalog, |call: Call, params: M::Params| async move {
        let params = serde_json::to_value(&params).unwrap_or(Value::Null);
        let profile = profile_for(&call, params.get("profile").and_then(Value::as_str))?;
        let client = Caller {
            server: Arc::downgrade(&call.server),
            conn: Arc::downgrade(&call.conn),
        };
        let value = call
            .server
            .host()
            .voice(
                &profile,
                VoiceCall {
                    method: M::NAME,
                    params,
                    client,
                },
            )
            .await
            .map_err(|e| rpc(&e))?;
        serde_json::from_value::<M::Result>(value).map_err(|e| {
            rpc(&pmt_core::error::Error::new(
                &pmt_core::error::codes::RPC_INTERNAL,
                format!(
                    "{}: the host's answer does not match the contract: {e}",
                    M::NAME
                ),
            ))
        })
    })?;
    Ok(())
}
