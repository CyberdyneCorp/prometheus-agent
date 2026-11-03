//! Sample `memory-provider` plugin: remembers the last turn in plugin
//! state and recalls it.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::memory_provider::plugin::{Guest, Recalled};
use bindings::prometheus::host::state;

struct Sample;

impl Guest for Sample {
    fn system_prompt_block() -> Option<String> {
        None
    }

    fn recall(_query: String, limit: u32) -> Result<Vec<Recalled>, String> {
        let last = state::get("last").map_err(|e| format!("{e:?}"))?;
        Ok(last
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(|text| Recalled { text, score: 1.0 })
            .into_iter()
            .take(limit as usize)
            .collect())
    }

    fn sync_turn(user: String, _assistant: String) -> Result<(), String> {
        state::set("last", user.as_bytes()).map_err(|e| format!("{e:?}"))
    }
}

bindings::export!(Sample with_types_in bindings);
