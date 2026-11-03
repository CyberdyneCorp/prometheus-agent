//! Sample `hook` plugin: blocks `pre_tool_call` for the `terminal` tool
//! and adds a line of context on `pre_llm_call`; its `extensions` add a
//! system-prompt section and a `/sample` command.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::hook::extensions::{self, Command};
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

impl extensions::Guest for Sample {
    fn sections() -> Vec<String> {
        vec!["sample.style".to_owned()]
    }

    fn render_section(_id: String) -> Result<String, String> {
        Ok("Prefer short sentences.".to_owned())
    }

    fn commands() -> Vec<Command> {
        vec![Command {
            name: "sample".to_owned(),
            description: "Says hello".to_owned(),
            surfaces: Vec::new(),
        }]
    }

    fn run_command(_name: String, args: String) -> Result<String, String> {
        Ok(format!("hello {args}"))
    }

    fn on_event(_name: String, _payload: String) -> Result<(), String> {
        Ok(())
    }
}

bindings::export!(Sample with_types_in bindings);
