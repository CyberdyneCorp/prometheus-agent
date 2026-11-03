//! Sample `tool` plugin: one `echo` tool that returns its arguments and
//! logs through the host.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::tool::plugin::{Guest, ToolSpec};
use bindings::prometheus::host::log::{Level, log};

struct Sample;

impl Guest for Sample {
    fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "echo".to_owned(),
            description: "Returns its arguments.".to_owned(),
            parameters: r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#
                .to_owned(),
        }]
    }

    fn call(name: String, arguments: String) -> Result<String, String> {
        log(Level::Debug, &format!("{name} called"));
        match name.as_str() {
            "echo" => Ok(arguments),
            other => Err(format!("unknown tool {other}")),
        }
    }
}

bindings::export!(Sample with_types_in bindings);
