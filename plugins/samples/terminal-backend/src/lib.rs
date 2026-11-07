//! Sample `terminal-backend` plugin: an in-memory "remote" whose commands
//! echo their line, and whose files live in the plugin's state store.

#[allow(warnings)]
mod bindings;

use bindings::exports::prometheus::terminal_backend::plugin::{
    Command, Execution, Guest, Output, Request,
};
use bindings::prometheus::host::state;

struct Sample;

impl Guest for Sample {
    fn run(command: Command) -> Result<Output, String> {
        Ok(Output {
            exit_code: 0,
            stdout: command.line,
            stderr: String::new(),
        })
    }

    fn name() -> String {
        "sample".to_owned()
    }

    fn descriptor() -> String {
        r#"{"isolation":"remote","uses_host_filesystem":false,"default_cwd":"/sandbox","session_isolated_when_ephemeral":true,"persistent":false}"#.to_owned()
    }

    fn check() -> Result<String, String> {
        Ok("in-memory sample".to_owned())
    }

    fn execute(request: Request) -> Result<Execution, String> {
        // `cd <dir>` moves the session; anything else is echoed.
        let cwd = request
            .line
            .strip_prefix("cd ")
            .map_or(request.cwd.clone(), |dir| dir.trim().to_owned());
        Ok(Execution {
            exit_code: Some(0),
            stdout: format!("{}\n", request.line),
            stderr: String::new(),
            cwd_observed: Some(cwd),
        })
    }

    fn upload(path: String, contents: Vec<u8>) -> Result<(), String> {
        state::set(&format!("file:{path}"), &contents).map_err(|e| format!("{e:?}"))
    }

    fn download(path: String) -> Result<Vec<u8>, String> {
        state::get(&format!("file:{path}"))
            .map_err(|e| format!("{e:?}"))?
            .ok_or_else(|| format!("{path}: not found"))
    }

    fn cancel(_id: String) {}

    fn teardown() {}
}

bindings::export!(Sample with_types_in bindings);
