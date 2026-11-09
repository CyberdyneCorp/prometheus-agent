//! Test plugin: a `terminal-backend` whose name, descriptor and replies
//! the test steers through its settings (`plugins.entries.<id>.*`):
//! `name`, `default_cwd`, `check_error`; a command line containing
//! `omit-exit-code` gets a reply without `exit_code`. A line containing
//! `wait-for-cancel` is a long remote command: it polls
//! `https://pause.example/<id>` (the test's port answers slowly) until
//! `cancel(<id>)` records `cancelled_<id>` in its settings, then exits 130
//! (it gives up after 60 s of polling).

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/credentials", "../../../wit/terminal-backend"],
    world: "prometheus:terminal-backend/terminal-backend",
    generate_all,
});

use exports::prometheus::terminal_backend::plugin::{Command, Execution, Guest, Output, Request};
use prometheus::host::{config, http};

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
        if request.line.contains("wait-for-cancel") {
            return wait_for_cancel(&request.id);
        }
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

    fn cancel(id: String) {
        let _ = config::set(&cancel_key(&id), "true");
    }

    fn teardown() {}
}

/// The setting `cancel(id)` records (setting keys are `[a-z0-9_]`).
fn cancel_key(id: &str) -> String {
    let id: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    format!("cancelled_{id}")
}

/// Waits on the "remote command" until it is cancelled.
fn wait_for_cancel(id: &str) -> Result<Execution, String> {
    let mut polls = 0_u32;
    while config::get(&cancel_key(id)).ok().flatten().is_none() {
        if polls >= 1200 {
            return Err("never cancelled".to_owned());
        }
        http::send(&http::Request {
            method: "GET".to_owned(),
            url: format!("https://pause.example/{id}"),
            headers: Vec::new(),
            body: None,
        })
        .map_err(|e| format!("{e:?}"))?;
        polls += 1;
    }
    Ok(Execution {
        exit_code: Some(130),
        stdout: format!("cancelled after {polls} polls\n"),
        stderr: String::new(),
        cwd_observed: None,
    })
}

export!(Backend);
