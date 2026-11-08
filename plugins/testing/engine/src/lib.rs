//! Test plugin: a context engine whose per-request selection keeps only
//! the last two messages, and whose compaction keeps the last message
//! after a one-line summary.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/context-engine"],
    world: "prometheus:context-engine/context-engine",
    generate_all,
});

use exports::prometheus::context_engine::plugin::{Guest, ToolSpec};

struct Engine;

/// The ids of a JSON message list, in order (test messages hold no
/// escaped quotes).
fn ids(messages: &str) -> Vec<String> {
    messages
        .split("\"id\":\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_owned)
        .collect()
}

impl Guest for Engine {
    fn initialize(_session: String, _profile: String) -> Result<(), String> {
        Ok(())
    }

    fn compact(messages: String) -> Result<String, String> {
        let ids = ids(&messages);
        let last = ids.last().ok_or("nothing to compact")?;
        Ok(format!(
            r#"[{{"role":"user","content":"Summary of earlier turns."}},{{"keep":"{last}"}}]"#
        ))
    }

    fn select(messages: String, _budget: u64) -> Result<Option<Vec<String>>, String> {
        let ids = ids(&messages);
        Ok(Some(ids.iter().rev().take(2).rev().cloned().collect()))
    }

    fn tool_schemas() -> Vec<ToolSpec> {
        Vec::new()
    }

    fn handle_tool_call(name: String, _arguments: String) -> Result<String, String> {
        Err(format!("no tool {name}"))
    }
}

export!(Engine);
