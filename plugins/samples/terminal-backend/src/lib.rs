//! Sample `terminal-backend` plugin: echoes the command line.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::terminal_backend::plugin::{Command, Guest, Output};

struct Sample;

impl Guest for Sample {
    fn run(command: Command) -> Result<Output, String> {
        Ok(Output {
            exit_code: 0,
            stdout: command.line,
            stderr: String::new(),
        })
    }
}

bindings::export!(Sample with_types_in bindings);
