//! Calls into the `platform-adapter` world (spec: platform-adapters,
//! Adapter contract; WASM adapter plugin isolation). Structured values
//! cross as the JSON forms of the host's adapter types; the gateway side
//! (`PluginAdapter` in the binary) parses them.
//!
//! Every call runs on the shared live store under the plugin's call
//! bounds, so a trap discards that store and the next call (the gateway's
//! reconnect) instantiates afresh.

use pmt_core::error::Error;

use super::super::platform_adapter_world::{Guest, HttpRequest, HttpResponse};
use super::{CallInfo, Instance};

/// The operations an adapter component must export (spec: platform-adapters,
/// Adapter contract): `start` and `stop` are the contract's `connect` and
/// `disconnect`.
pub(crate) const REQUIRED_OPERATIONS: &[&str] = &[
    "descriptor",
    "start",
    "stop",
    "poll",
    "send",
    "chat-info",
    "handle-http",
    "authorize-sender",
];

impl Instance {
    /// `plugin.descriptor()`: the descriptor as JSON.
    ///
    /// # Errors
    ///
    /// `plugins.load` when the component does not export the world;
    /// `plugins.call` when the call traps or overruns.
    pub fn adapter_descriptor(&self, call: &CallInfo) -> Result<String, Error> {
        self.adapter_call(call, |guest, store| guest.call_descriptor(store))
    }

    /// `plugin.start(options)` (connect).
    ///
    /// # Errors
    ///
    /// As [`Self::adapter_descriptor`].
    pub fn adapter_start(
        &self,
        call: &CallInfo,
        options: &str,
    ) -> Result<Result<(), String>, Error> {
        self.adapter_call(call, |guest, store| guest.call_start(store, options))
    }

    /// `plugin.stop()` (disconnect).
    ///
    /// # Errors
    ///
    /// As [`Self::adapter_descriptor`].
    pub fn adapter_stop(&self, call: &CallInfo) -> Result<(), Error> {
        self.adapter_call(call, |guest, store| guest.call_stop(store))
    }

    /// `plugin.poll()`: the inbound messages since the last poll (JSON).
    ///
    /// # Errors
    ///
    /// As [`Self::adapter_descriptor`].
    pub fn adapter_poll(&self, call: &CallInfo) -> Result<Result<String, String>, Error> {
        self.adapter_call(call, |guest, store| guest.call_poll(store))
    }

    /// `plugin.send(message)`: the platform message id.
    ///
    /// # Errors
    ///
    /// As [`Self::adapter_descriptor`].
    pub fn adapter_send(
        &self,
        call: &CallInfo,
        message: &str,
    ) -> Result<Result<String, String>, Error> {
        self.adapter_call(call, |guest, store| guest.call_send(store, message))
    }

    /// `plugin.chat-info(chat)`: `{name, chat_type}` as JSON.
    ///
    /// # Errors
    ///
    /// As [`Self::adapter_descriptor`].
    pub fn adapter_chat_info(
        &self,
        call: &CallInfo,
        chat: &str,
    ) -> Result<Result<String, String>, Error> {
        self.adapter_call(call, |guest, store| guest.call_chat_info(store, chat))
    }

    /// `plugin.handle-http(request)`.
    ///
    /// # Errors
    ///
    /// As [`Self::adapter_descriptor`].
    pub fn adapter_handle_http(
        &self,
        call: &CallInfo,
        request: &HttpRequest,
    ) -> Result<HttpResponse, Error> {
        self.adapter_call(call, |guest, store| guest.call_handle_http(store, request))
    }

    /// `plugin.authorize-sender(inbound)`: `allow`, `deny` or anything
    /// else for unknown.
    ///
    /// # Errors
    ///
    /// As [`Self::adapter_descriptor`].
    pub fn adapter_authorize(&self, call: &CallInfo, inbound: &str) -> Result<String, Error> {
        self.adapter_call(call, |guest, store| {
            guest.call_authorize_sender(store, inbound)
        })
    }

    fn adapter_call<R>(
        &self,
        call: &CallInfo,
        f: impl FnOnce(
            &Guest,
            &mut wasmtime::Store<super::super::imports::HostState>,
        ) -> wasmtime::Result<R>,
    ) -> Result<R, Error> {
        let indices = self
            .adapter
            .as_ref()
            .ok_or_else(|| self.missing("platform-adapter"))?;
        self.with_live(call, None, |store, instance| {
            let guest = indices.load(&mut *store, instance)?;
            f(&guest, store)
        })
    }
}
