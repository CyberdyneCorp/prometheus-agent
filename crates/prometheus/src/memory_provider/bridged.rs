//! A Hermes `MemoryProvider` in its plugin's bridge sidecar behind
//! [`ProviderBackend`] (spec: python-bridge, Scope of hosted code): the
//! same calls as the WASM world, as named JSON arguments.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_memory::provider::{Mirrored, ProviderBackend, ProviderTool, SessionInfo, SetupField};
use pmt_plugins::memory::{ExternalMemory, MemoryCalls};
use serde_json::{Value, json};

/// One bridged provider of a profile.
pub struct BridgedBackend {
    id: String,
    calls: Arc<dyn MemoryCalls>,
    session: Mutex<Option<String>>,
}

impl BridgedBackend {
    /// The backend of `external`.
    #[must_use]
    pub fn new(external: &ExternalMemory) -> Self {
        Self {
            id: external.plugin.clone(),
            calls: Arc::clone(&external.calls),
            session: Mutex::new(None),
        }
    }

    fn call(
        &self,
        method: &str,
        mut args: Value,
        bound: Option<Duration>,
    ) -> Result<Value, String> {
        if let (Value::Object(map), Some(session)) = (
            &mut args,
            self.session
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        ) {
            map.entry("session_id").or_insert(json!(session));
        }
        self.calls
            .call(method, &args, bound)
            .map_err(|e| e.message().to_owned())?
    }

    fn unit(&self, method: &str, args: Value) -> Result<(), String> {
        self.call(method, args, None).map(drop)
    }

    fn text(value: &Value) -> Option<String> {
        value.as_str().map(str::to_owned).filter(|s| !s.is_empty())
    }
}

impl ProviderBackend for BridgedBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> String {
        self.call("name", json!({}), None)
            .ok()
            .as_ref()
            .and_then(Self::text)
            .unwrap_or_else(|| self.id.clone())
    }

    fn is_available(&self) -> bool {
        self.call("is_available", json!({}), None)
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn unavailable_reason(&self) -> Option<String> {
        self.call("unavailable_reason", json!({}), None)
            .ok()
            .as_ref()
            .and_then(Self::text)
    }

    fn initialize(&self, session: &SessionInfo) -> Result<(), String> {
        let args = json!({
            "session_id": session.session_id,
            "profile": session.profile,
            "platform": session.platform,
            "user_id": session.user_id,
        });
        let switching = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|s| *s != session.session_id);
        let method = if switching {
            "on_session_switch"
        } else {
            "initialize"
        };
        self.call(method, args, None)?;
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(session.session_id.clone());
        Ok(())
    }

    fn tool_schemas(&self) -> Vec<ProviderTool> {
        let Ok(Value::Array(specs)) = self.call("tool_schemas", json!({}), None) else {
            return Vec::new();
        };
        specs
            .iter()
            .map(|s| ProviderTool {
                name: s["name"].as_str().unwrap_or_default().to_owned(),
                description: s["description"].as_str().unwrap_or_default().to_owned(),
                parameters: s["parameters"].to_string(),
            })
            .filter(|t| !t.name.is_empty())
            .collect()
    }

    fn prompt_block(&self) -> Option<String> {
        self.call("system_prompt_block", json!({}), None)
            .ok()
            .as_ref()
            .and_then(Self::text)
    }

    fn prefetch(&self, query: &str, bound: Duration) -> Result<Option<String>, String> {
        self.call("prefetch", json!({"query": query}), Some(bound))
            .map(|v| Self::text(&v))
    }

    fn queue_prefetch(&self, query: &str) -> Result<(), String> {
        self.unit("queue_prefetch", json!({"query": query}))
    }

    fn sync_turn(&self, user: &str, assistant: &str) -> Result<(), String> {
        self.unit("sync_turn", json!({"user": user, "assistant": assistant}))
    }

    fn tool_call(&self, name: &str, arguments: &str) -> Result<String, String> {
        let arguments: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
        self.call(
            "handle_tool_call",
            json!({"name": name, "arguments": arguments}),
            None,
        )
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
    }

    fn turn_start(&self, text: &str) -> Result<(), String> {
        self.unit("on_turn_start", json!({"text": text}))
    }

    fn session_end(&self) -> Result<(), String> {
        self.unit("on_session_end", json!({}))
    }

    fn pre_compress(&self, messages: &str) -> Result<bool, String> {
        let messages: Value = serde_json::from_str(messages).unwrap_or_else(|_| json!([]));
        self.call("on_pre_compress", json!({"messages": messages}), None)
            .map(|v| v.as_bool().unwrap_or(false))
    }

    fn delegation(&self, task: &str) -> Result<(), String> {
        self.unit("on_delegation", json!({"task": task}))
    }

    fn memory_write(&self, write: &Mirrored) -> Result<(), String> {
        self.unit(
            "on_memory_write",
            json!({
                "action": write.action,
                "target": write.target,
                "entry_id": write.entry_id,
                "content": write.content,
                "previous_content": write.previous,
            }),
        )
    }

    fn config_schema(&self) -> Vec<SetupField> {
        let Ok(Value::Array(fields)) = self.call("config_schema", json!({}), None) else {
            return Vec::new();
        };
        fields
            .iter()
            .map(|f| SetupField {
                key: f["key"].as_str().unwrap_or_default().to_owned(),
                description: f["description"].as_str().unwrap_or_default().to_owned(),
                secret: f["secret"].as_bool().unwrap_or(false),
                required: f["required"].as_bool().unwrap_or(false),
                default: f["default"].as_str().map(str::to_owned),
            })
            .filter(|f| !f.key.is_empty())
            .collect()
    }

    fn post_setup(&self) -> Result<(), String> {
        self.unit("post_setup", json!({}))
    }
}
