//! A `memory-provider` WASM plugin behind [`ProviderBackend`]. A trap, a
//! timeout or a missing world becomes the call's error text; the host side
//! ([`pmt_memory::provider`]) logs it and goes on.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_core::error::Error;
use pmt_memory::provider::{Mirrored, ProviderBackend, ProviderTool, SessionInfo, SetupField};
use pmt_plugins::host::memory_provider_types as wit;
use pmt_plugins::host::{CallInfo, Instance, MemoryWrite};

/// One provider plugin instance of a profile.
pub struct WasmBackend {
    instance: Arc<Instance>,
    call: CallInfo,
    /// The session the instance serves now: a second session of the same
    /// instance gets `on-session-switch` instead of `initialize`.
    bound: Mutex<Option<String>>,
}

impl WasmBackend {
    /// The backend of `instance`, calling it as `call` reports.
    #[must_use]
    pub fn new(instance: Arc<Instance>, call: CallInfo) -> Self {
        Self {
            instance,
            call,
            bound: Mutex::new(None),
        }
    }
}

fn flat<T>(result: Result<Result<T, String>, Error>) -> Result<T, String> {
    result.map_err(|e| e.message().to_owned())?
}

fn info(session: &SessionInfo) -> wit::SessionInfo {
    wit::SessionInfo {
        session_id: session.session_id.clone(),
        profile: session.profile.clone(),
        platform: session.platform.clone(),
        user_id: session.user_id.clone(),
    }
}

impl ProviderBackend for WasmBackend {
    fn id(&self) -> &str {
        self.instance.id()
    }

    fn name(&self) -> String {
        self.instance
            .memory_name(&self.call)
            .unwrap_or_else(|_| self.instance.id().to_owned())
    }

    fn is_available(&self) -> bool {
        self.instance
            .memory_is_available(&self.call)
            .unwrap_or(false)
    }

    fn unavailable_reason(&self) -> Option<String> {
        self.instance
            .memory_unavailable_reason(&self.call)
            .ok()
            .flatten()
    }

    fn initialize(&self, session: &SessionInfo) -> Result<(), String> {
        let mut bound = self.bound.lock().unwrap_or_else(PoisonError::into_inner);
        let result = match bound.as_deref() {
            Some(other) if other != session.session_id => flat(
                self.instance
                    .memory_session_switch(&self.call, &info(session)),
            ),
            _ => flat(self.instance.memory_initialize(&self.call, &info(session))),
        };
        if result.is_ok() {
            *bound = Some(session.session_id.clone());
        }
        result
    }

    fn tool_schemas(&self) -> Vec<ProviderTool> {
        self.instance
            .memory_tool_schemas(&self.call)
            .unwrap_or_default()
            .into_iter()
            .map(|t| ProviderTool {
                name: t.name,
                description: t.description,
                parameters: t.parameters,
            })
            .collect()
    }

    fn prompt_block(&self) -> Option<String> {
        self.instance.memory_prompt_block(&self.call).ok().flatten()
    }

    fn prefetch(&self, query: &str, bound: Duration) -> Result<Option<String>, String> {
        match self.instance.memory_prefetch(&self.call, query, bound) {
            Err(err) if err.message().ends_with("exceeded call timeout") => Err(format!(
                "{} after {} ms",
                pmt_memory::provider::TIMED_OUT,
                bound.as_millis()
            )),
            other => flat(other),
        }
    }

    fn queue_prefetch(&self, query: &str) -> Result<(), String> {
        flat(self.instance.memory_queue_prefetch(&self.call, query))
    }

    fn sync_turn(&self, user: &str, assistant: &str) -> Result<(), String> {
        flat(self.instance.memory_sync_turn(&self.call, user, assistant))
    }

    fn tool_call(&self, name: &str, arguments: &str) -> Result<String, String> {
        flat(self.instance.memory_tool_call(&self.call, name, arguments))
    }

    fn turn_start(&self, text: &str) -> Result<(), String> {
        flat(self.instance.memory_turn_start(&self.call, text))
    }

    fn session_end(&self) -> Result<(), String> {
        flat(self.instance.memory_session_end(&self.call))
    }

    fn pre_compress(&self, messages: &str) -> Result<bool, String> {
        flat(self.instance.memory_pre_compress(&self.call, messages))
    }

    fn delegation(&self, task: &str) -> Result<(), String> {
        flat(self.instance.memory_delegation(&self.call, task))
    }

    fn memory_write(&self, write: &Mirrored) -> Result<(), String> {
        let write = MemoryWrite {
            action: &write.action,
            target: &write.target,
            entry_id: &write.entry_id,
            content: &write.content,
            previous: write.previous.as_deref(),
        };
        flat(self.instance.memory_write(&self.call, &write))
    }

    fn config_schema(&self) -> Vec<SetupField> {
        self.instance
            .memory_config_schema(&self.call)
            .unwrap_or_default()
            .into_iter()
            .map(|f| SetupField {
                key: f.key,
                description: f.description,
                secret: f.secret,
                required: f.required,
                default: f.default_value,
            })
            .collect()
    }

    fn post_setup(&self) -> Result<(), String> {
        flat(self.instance.memory_post_setup(&self.call))
    }
}
