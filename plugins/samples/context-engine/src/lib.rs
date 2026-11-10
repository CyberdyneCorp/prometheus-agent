//! Sample `context-engine` plugin: compacts nothing it cannot keep (every
//! message is kept) and sends every message.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::context_engine::plugin::{Guest, ToolSpec};

struct Sample;

impl Guest for Sample {
    fn initialize(_session: String, _profile: String) -> Result<(), String> {
        Ok(())
    }

    fn compact(_messages: String) -> Result<String, String> {
        Err("this sample engine never compacts".to_owned())
    }

    fn select(_messages: String, _budget: u64) -> Result<Option<Vec<String>>, String> {
        Ok(None)
    }

    fn tool_schemas() -> Vec<ToolSpec> {
        Vec::new()
    }

    fn handle_tool_call(name: String, _arguments: String) -> Result<String, String> {
        Err(format!("no tool {name}"))
    }
}

bindings::export!(Sample with_types_in bindings);
