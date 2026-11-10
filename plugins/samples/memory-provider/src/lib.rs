//! Sample `memory-provider` plugin: keeps each synced turn in plugin state
//! and recalls the stored turns that share a word with the query. It
//! exports both interfaces of the world: `plugin` (required) and `hooks`
//! (optional; a provider without it gets no-ops).

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::memory_provider::hooks::Guest as Hooks;
use bindings::exports::prometheus::memory_provider::plugin::Guest;
use bindings::prometheus::host::state;
use bindings::prometheus::memory_provider::types::{ConfigField, SessionInfo, ToolSpec};

struct Sample;

fn turns() -> Vec<String> {
    state::get("turns")
        .ok()
        .flatten()
        .map(|b| String::from_utf8_lossy(&b).lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

impl Guest for Sample {
    fn name() -> String {
        "Sample memory".to_owned()
    }

    fn is_available() -> bool {
        true
    }

    fn initialize(_session: SessionInfo) -> Result<(), String> {
        Ok(())
    }

    fn tool_schemas() -> Vec<ToolSpec> {
        Vec::new()
    }
}

impl Hooks for Sample {
    fn system_prompt_block() -> Option<String> {
        None
    }

    fn prefetch(query: String) -> Result<Option<String>, String> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let hits: Vec<String> = turns()
            .into_iter()
            .filter(|t| words.iter().any(|w| t.to_lowercase().contains(w.as_str())))
            .collect();
        Ok((!hits.is_empty()).then(|| hits.join("\n")))
    }

    fn queue_prefetch(_query: String) -> Result<(), String> {
        Ok(())
    }

    fn sync_turn(user: String, _assistant: String) -> Result<(), String> {
        let mut all = turns();
        all.push(user.replace('\n', " "));
        state::set("turns", all.join("\n").as_bytes()).map_err(|e| format!("{e:?}"))
    }

    fn handle_tool_call(name: String, _arguments: String) -> Result<String, String> {
        Err(format!("no tool {name}"))
    }

    fn on_turn_start(_text: String) -> Result<(), String> {
        Ok(())
    }

    fn on_session_end() -> Result<(), String> {
        Ok(())
    }

    fn on_session_switch(_session: SessionInfo) -> Result<(), String> {
        Ok(())
    }

    fn on_pre_compress(_messages: String) -> Result<bool, String> {
        Ok(true)
    }

    fn on_delegation(_task: String) -> Result<(), String> {
        Ok(())
    }

    fn on_memory_write(
        _action: String,
        _target: String,
        _entry_id: String,
        _content: String,
        _previous_content: Option<String>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn config_schema() -> Vec<ConfigField> {
        Vec::new()
    }

    fn post_setup() -> Result<(), String> {
        Ok(())
    }

    fn recall_status() -> Option<String> {
        None
    }

    fn unavailable_reason() -> Option<String> {
        None
    }
}

bindings::export!(Sample with_types_in bindings);
