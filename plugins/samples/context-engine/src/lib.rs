//! Sample `context-engine` plugin: never compresses.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::context_engine::plugin::Guest;

struct Sample;

impl Guest for Sample {
    fn should_compress(prompt_tokens: u64, context_window: u64) -> bool {
        prompt_tokens > context_window
    }

    fn compress(messages: String, _budget_tokens: u64) -> Result<String, String> {
        Ok(messages)
    }
}

bindings::export!(Sample with_types_in bindings);
