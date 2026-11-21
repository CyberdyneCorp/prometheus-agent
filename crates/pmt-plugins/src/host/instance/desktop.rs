//! Calls into the `computer-use-backend` world (spec: computer-use,
//! Backend selection and availability).
//!
//! Every call runs on a pooled store, like a terminal backend's: actions of
//! concurrent sessions reach the plugin at the same time, and a call
//! waiting on the remote desktop is bounded by the CPU budget only.

use pmt_core::error::Error;

use super::super::computer_use_world::{Guest, Reply, RuntimeSpec};
use super::super::imports::HostState;
use super::{CallInfo, Instance};

/// The world as `plugin.toml` names it.
const WORLD: &str = "computer-use-backend";

impl Instance {
    /// `plugin.name()`.
    ///
    /// # Errors
    ///
    /// `plugins.load` when the component does not export the world;
    /// `plugins.call` when the call traps or overruns.
    pub fn desktop_name(&self, call: &CallInfo) -> Result<String, Error> {
        self.desktop_call(call, |guest, store| guest.call_name(store))
    }

    /// `plugin.check()`.
    ///
    /// # Errors
    ///
    /// As [`Self::desktop_name`].
    pub fn desktop_check(&self, call: &CallInfo) -> Result<Result<String, String>, Error> {
        self.desktop_call(call, |guest, store| guest.call_check(store))
    }

    /// `plugin.launch(spec)`: the runtime id.
    ///
    /// # Errors
    ///
    /// As [`Self::desktop_name`].
    pub fn desktop_launch(
        &self,
        call: &CallInfo,
        spec: &RuntimeSpec,
    ) -> Result<Result<String, String>, Error> {
        self.desktop_call(call, |guest, store| guest.call_launch(store, spec))
    }

    /// `plugin.tools(runtime)`.
    ///
    /// # Errors
    ///
    /// As [`Self::desktop_name`].
    pub fn desktop_tools(&self, call: &CallInfo, runtime: &str) -> Result<Vec<String>, Error> {
        self.desktop_call(call, |guest, store| guest.call_tools(store, runtime))
    }

    /// `plugin.call(runtime, tool, arguments)`.
    ///
    /// # Errors
    ///
    /// As [`Self::desktop_name`].
    pub fn desktop_tool_call(
        &self,
        call: &CallInfo,
        runtime: &str,
        tool: &str,
        arguments: &str,
    ) -> Result<Result<Reply, String>, Error> {
        self.desktop_call(call, |guest, store| {
            guest.call_call(store, runtime, tool, arguments)
        })
    }

    /// `plugin.stop(runtime)`.
    ///
    /// # Errors
    ///
    /// As [`Self::desktop_name`].
    pub fn desktop_stop(&self, call: &CallInfo, runtime: &str) -> Result<(), Error> {
        self.desktop_call(call, |guest, store| guest.call_stop(store, runtime))
    }

    fn desktop_call<R>(
        &self,
        call: &CallInfo,
        f: impl FnOnce(&Guest, &mut wasmtime::Store<HostState>) -> wasmtime::Result<R>,
    ) -> Result<R, Error> {
        let indices = self.desktop.as_ref().ok_or_else(|| self.missing(WORLD))?;
        self.with_pooled(call, None, |store, instance| {
            let guest = indices.load(&mut *store, instance)?;
            f(&guest, store)
        })
    }
}
