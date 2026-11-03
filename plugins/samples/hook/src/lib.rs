//! Sample `hook` plugin: blocks `pre_tool_call` for the `terminal` tool
//! and adds a line of context on `pre_llm_call`; its `extensions` add a
//! system-prompt section and a `/sample` command; its `surfaces` add a
//! `prometheus plugins run <id> hello` command.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::hook::extensions::{self, Command};
use bindings::exports::prometheus::hook::plugin::{Guest, HookEvent, Reply};
use bindings::exports::prometheus::hook::surfaces::{
    self, ApprovalRequest, ApprovalResponse, CliCommand, CliOutput, Terminal,
};

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

impl surfaces::Guest for Sample {
    fn hold_stream() -> Vec<String> {
        Vec::new()
    }

    fn cli_commands() -> Vec<CliCommand> {
        vec![CliCommand {
            name: "hello".to_owned(),
            description: "Greets whoever is named".to_owned(),
        }]
    }

    fn run_cli(_name: String, args: Vec<String>, _terminal: Terminal) -> Result<CliOutput, String> {
        Ok(CliOutput {
            code: 0,
            stdout: format!("hello {}\n", args.join(" ")).into_bytes(),
            stderr: Vec::new(),
        })
    }

    fn transports() -> Vec<String> {
        Vec::new()
    }

    fn present_approval(
        name: String,
        _request: ApprovalRequest,
    ) -> Result<ApprovalResponse, String> {
        Err(format!("no transport named {name}"))
    }
}

bindings::export!(Sample with_types_in bindings);
