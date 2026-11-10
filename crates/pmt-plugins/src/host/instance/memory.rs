//! Calls into the `memory-provider` world (spec: memory, External memory
//! provider contract). `plugin` is required; every `hooks` function is
//! optional as a whole: a component that does not export `hooks` answers
//! each of them with its no-op value.

use std::time::Duration;

use pmt_core::error::Error;

use super::super::bindings::prometheus::memory_provider::types::{
    ConfigField, SessionInfo, ToolSpec,
};
use super::{CallInfo, Instance};

/// What a call of an optional hook returns when the plugin has no hooks.
type Hooked<T> = Result<T, Error>;

impl Instance {
    fn memory_plugin(&self) -> Result<&super::memory_provider::plugin::GuestIndices, Error> {
        self.memory
            .as_ref()
            .ok_or_else(|| self.missing("memory-provider"))
    }

    /// `plugin.name()`.
    ///
    /// # Errors
    ///
    /// `plugins.load` without the world; `plugins.call` on a trap or
    /// timeout.
    pub fn memory_name(&self, call: &CallInfo) -> Result<String, Error> {
        let indices = self.memory_plugin()?;
        self.with_live(call, None, |store, instance| {
            indices.load(&mut *store, instance)?.call_name(&mut *store)
        })
    }

    /// `plugin.is-available()`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_name`].
    pub fn memory_is_available(&self, call: &CallInfo) -> Result<bool, Error> {
        let indices = self.memory_plugin()?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_is_available(&mut *store)
        })
    }

    /// `plugin.initialize(session)`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_name`].
    pub fn memory_initialize(
        &self,
        call: &CallInfo,
        session: &SessionInfo,
    ) -> Result<Result<(), String>, Error> {
        let indices = self.memory_plugin()?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_initialize(&mut *store, session)
        })
    }

    /// `plugin.tool-schemas()`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_name`].
    pub fn memory_tool_schemas(&self, call: &CallInfo) -> Result<Vec<ToolSpec>, Error> {
        let indices = self.memory_plugin()?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_tool_schemas(&mut *store)
        })
    }

    /// Runs one `hooks` call under `bound`, or answers `absent` when the
    /// plugin exports no hooks.
    fn memory_hook<T>(
        &self,
        call: &CallInfo,
        bound: Option<Duration>,
        absent: T,
        f: impl FnOnce(
            &super::memory_provider::hooks::Guest,
            &mut wasmtime::Store<super::HostState>,
        ) -> wasmtime::Result<T>,
    ) -> Hooked<T> {
        let Some(indices) = self.memory_hooks.as_ref() else {
            return Ok(absent);
        };
        self.with_live(call, bound, |store, instance| {
            let hooks = indices.load(&mut *store, instance)?;
            f(&hooks, store)
        })
    }

    /// `hooks.system-prompt-block()`.
    ///
    /// # Errors
    ///
    /// `plugins.call` on a trap or timeout.
    pub fn memory_prompt_block(&self, call: &CallInfo) -> Hooked<Option<String>> {
        self.memory_hook(call, None, None, |h, s| h.call_system_prompt_block(s))
    }

    /// `hooks.prefetch(query)` within `bound`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_prefetch(
        &self,
        call: &CallInfo,
        query: &str,
        bound: Duration,
    ) -> Hooked<Result<Option<String>, String>> {
        self.memory_hook(call, Some(bound), Ok(None), |h, s| {
            h.call_prefetch(s, query)
        })
    }

    /// `hooks.queue-prefetch(query)`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_queue_prefetch(
        &self,
        call: &CallInfo,
        query: &str,
    ) -> Hooked<Result<(), String>> {
        self.memory_hook(call, None, Ok(()), |h, s| h.call_queue_prefetch(s, query))
    }

    /// `hooks.sync-turn(user, assistant)`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_sync_turn(
        &self,
        call: &CallInfo,
        user: &str,
        assistant: &str,
    ) -> Hooked<Result<(), String>> {
        self.memory_hook(call, None, Ok(()), |h, s| {
            h.call_sync_turn(s, user, assistant)
        })
    }

    /// `hooks.handle-tool-call(name, arguments)`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_tool_call(
        &self,
        call: &CallInfo,
        name: &str,
        arguments: &str,
    ) -> Hooked<Result<String, String>> {
        let absent = Err(format!("the provider does not handle '{name}'"));
        self.memory_hook(call, None, absent, |h, s| {
            h.call_handle_tool_call(s, name, arguments)
        })
    }

    /// `hooks.on-turn-start(text)`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_turn_start(&self, call: &CallInfo, text: &str) -> Hooked<Result<(), String>> {
        self.memory_hook(call, None, Ok(()), |h, s| h.call_on_turn_start(s, text))
    }

    /// `hooks.on-session-end()`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_session_end(&self, call: &CallInfo) -> Hooked<Result<(), String>> {
        self.memory_hook(call, None, Ok(()), |h, s| h.call_on_session_end(s))
    }

    /// `hooks.on-session-switch(session)`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_session_switch(
        &self,
        call: &CallInfo,
        session: &SessionInfo,
    ) -> Hooked<Result<(), String>> {
        self.memory_hook(call, None, Ok(()), |h, s| {
            h.call_on_session_switch(s, session)
        })
    }

    /// `hooks.on-pre-compress(messages)`: whether the checkpoint was
    /// confirmed. Without hooks nothing is confirmed.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_pre_compress(
        &self,
        call: &CallInfo,
        messages: &str,
    ) -> Hooked<Result<bool, String>> {
        self.memory_hook(call, None, Ok(false), |h, s| {
            h.call_on_pre_compress(s, messages)
        })
    }

    /// `hooks.on-delegation(task)`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_delegation(&self, call: &CallInfo, task: &str) -> Hooked<Result<(), String>> {
        self.memory_hook(call, None, Ok(()), |h, s| h.call_on_delegation(s, task))
    }

    /// `hooks.on-memory-write(...)`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_write(
        &self,
        call: &CallInfo,
        write: &MemoryWrite<'_>,
    ) -> Hooked<Result<(), String>> {
        self.memory_hook(call, None, Ok(()), |h, s| {
            h.call_on_memory_write(
                s,
                write.action,
                write.target,
                write.entry_id,
                write.content,
                write.previous,
            )
        })
    }

    /// `hooks.config-schema()`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_config_schema(&self, call: &CallInfo) -> Hooked<Vec<ConfigField>> {
        self.memory_hook(call, None, Vec::new(), |h, s| h.call_config_schema(s))
    }

    /// `hooks.post-setup()`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_post_setup(&self, call: &CallInfo) -> Hooked<Result<(), String>> {
        self.memory_hook(call, None, Ok(()), |h, s| h.call_post_setup(s))
    }

    /// `hooks.recall-status()`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_recall_status(&self, call: &CallInfo) -> Hooked<Option<String>> {
        self.memory_hook(call, None, None, |h, s| h.call_recall_status(s))
    }

    /// `hooks.unavailable-reason()`.
    ///
    /// # Errors
    ///
    /// As [`Self::memory_prompt_block`].
    pub fn memory_unavailable_reason(&self, call: &CallInfo) -> Hooked<Option<String>> {
        self.memory_hook(call, None, None, |h, s| h.call_unavailable_reason(s))
    }
}

/// One committed built-in memory operation, mirrored to the provider.
#[derive(Debug, Clone, Copy)]
pub struct MemoryWrite<'a> {
    /// `add`, `patch`, `remove`...
    pub action: &'a str,
    /// `memory` or `user`.
    pub target: &'a str,
    /// The entry.
    pub entry_id: &'a str,
    /// Its content after the operation (empty for a removal).
    pub content: &'a str,
    /// Its content before, from the committed revision.
    pub previous: Option<&'a str>,
}
