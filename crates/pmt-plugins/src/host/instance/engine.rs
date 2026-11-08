//! Calls into the `context-engine` world (spec: context-compression,
//! Context engine selection; memory, Context engine selection).

use pmt_core::error::Error;

use super::super::bindings::exports::prometheus::context_engine::plugin::ToolSpec;
use super::{CallInfo, Instance};

impl Instance {
    fn engine_plugin(&self) -> Result<&super::context_engine::plugin::GuestIndices, Error> {
        self.engine
            .as_ref()
            .ok_or_else(|| self.missing("context-engine"))
    }

    /// `plugin.initialize(session, profile)`.
    ///
    /// # Errors
    ///
    /// `plugins.load` without the world; `plugins.call` on a trap or
    /// timeout.
    pub fn engine_initialize(
        &self,
        call: &CallInfo,
        session: &str,
        profile: &str,
    ) -> Result<Result<(), String>, Error> {
        let indices = self.engine_plugin()?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_initialize(&mut *store, session, profile)
        })
    }

    /// `plugin.compact(messages)`.
    ///
    /// # Errors
    ///
    /// As [`Self::engine_initialize`].
    pub fn engine_compact(
        &self,
        call: &CallInfo,
        messages: &str,
    ) -> Result<Result<String, String>, Error> {
        let indices = self.engine_plugin()?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_compact(&mut *store, messages)
        })
    }

    /// `plugin.select(messages, budget)`.
    ///
    /// # Errors
    ///
    /// As [`Self::engine_initialize`].
    pub fn engine_select(
        &self,
        call: &CallInfo,
        messages: &str,
        budget: u64,
    ) -> Result<Result<Option<Vec<String>>, String>, Error> {
        let indices = self.engine_plugin()?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_select(&mut *store, messages, budget)
        })
    }

    /// `plugin.tool-schemas()`.
    ///
    /// # Errors
    ///
    /// As [`Self::engine_initialize`].
    pub fn engine_tool_schemas(&self, call: &CallInfo) -> Result<Vec<ToolSpec>, Error> {
        let indices = self.engine_plugin()?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_tool_schemas(&mut *store)
        })
    }

    /// `plugin.handle-tool-call(name, arguments)`.
    ///
    /// # Errors
    ///
    /// As [`Self::engine_initialize`].
    pub fn engine_tool_call(
        &self,
        call: &CallInfo,
        name: &str,
        arguments: &str,
    ) -> Result<Result<String, String>, Error> {
        let indices = self.engine_plugin()?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_handle_tool_call(&mut *store, name, arguments)
        })
    }
}
