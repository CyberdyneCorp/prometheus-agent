//! Test plugin: a `terminal-backend` whose name, descriptor and replies
//! the test steers through its settings (`plugins.entries.<id>.*`):
//! `name`, `default_cwd`, `check_error`; a command line containing
//! `omit-exit-code` gets a reply without `exit_code`.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/terminal-backend"],
    world: "prometheus:terminal-backend/terminal-backend",
    generate_all,
});

use exports::prometheus::terminal_backend::plugin::{Command, Execution, Guest, Output, Request};
use prometheus::host::config;

struct Backend;

/// A string setting (stored as a JSON string).
fn setting(key: &str) -> Option<String> {
    config::get(key)
        .ok()
        .flatten()
        .map(|v| v.trim_matches('"').to_owned())
}

impl Guest for Backend {
    fn run(_: Command) -> Result<Output, String> {
        Err("1.0 call".to_owned())
    }

    fn name() -> String {
        setting("name").unwrap_or_else(|| "testbox".to_owned())
    }

    fn descriptor() -> String {
        let cwd = setting("default_cwd").unwrap_or_else(|| "/box".to_owned());
        format!(
            r#"{{"isolation":"remote","uses_host_filesystem":false,"default_cwd":"{cwd}","session_isolated_when_ephemeral":true,"persistent":false}}"#
        )
    }

    fn check() -> Result<String, String> {
        match setting("check_error") {
            Some(why) => Err(why),
            None => Ok("test backend".to_owned()),
        }
    }

    fn execute(request: Request) -> Result<Execution, String> {
        let env: Vec<String> = request.env.iter().map(|(k, _)| k.clone()).collect();
        Ok(Execution {
            exit_code: (!request.line.contains("omit-exit-code")).then_some(3),
            stdout: format!("ran: {} in {} env: {}\n", request.line, request.cwd, env.join(",")),
            stderr: String::new(),
            cwd_observed: Some(request.cwd),
        })
    }

    fn upload(_: String, _: Vec<u8>) -> Result<(), String> {
        Ok(())
    }

    fn download(path: String) -> Result<Vec<u8>, String> {
        Ok(format!("contents of {path}").into_bytes())
    }

    fn cancel(_: String) {}

    fn teardown() {}
}

export!(Backend);
