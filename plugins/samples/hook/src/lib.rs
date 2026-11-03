//! Sample `hook` plugin: blocks `pre_tool_call` for the `terminal` tool
//! and adds a line of context on `pre_llm_call`.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::hook::plugin::{Guest, HookEvent, Reply};

struct Sample;

impl Guest for Sample {
    fn hooks() -> Vec<String> {
        vec!["pre_tool_call".to_owned(), "pre_llm_call".to_owned()]
    }

    fn on_hook(event: HookEvent) -> Result<Reply, String> {
        Ok(match event.hook.as_str() {
            "pre_tool_call" if event.payload.contains(r#""tool":"terminal""#) => {
                Reply::Block("the sample hook blocks the terminal".to_owned())
            }
            "pre_llm_call" => Reply::Context("Answer briefly.".to_owned()),
            _ => Reply::None,
        })
    }
}

bindings::export!(Sample with_types_in bindings);
