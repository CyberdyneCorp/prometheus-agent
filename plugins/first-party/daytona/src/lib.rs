//! First-party `terminal-backend` plugin for Daytona sandboxes (spec:
//! execution-environments, First-party backend plugins; Remote sandboxes
//! are plugins).
//!
//! One sandbox per session, created on first use (from the `snapshot`
//! setting when set), labelled with the session id and kept in the plugin's
//! state store so every instance finds it. `teardown` deletes it, or stops
//! it when the `persistent` setting is true (the next session's first
//! command starts it again).
//!
//! Each command runs in a toolbox session named after the request id, as
//! an asynchronous command, so `cancel` (on another instance) can delete
//! that session while `execute` still waits. The wait follows the
//! command's logs: one follow request returns when the command ends or the
//! host's request bound elapses, and is then repeated. Daytona merges
//! stdout and stderr; both come back as `stdout`.
//!
//! Settings (`plugins.entries.daytona.*`): `api_url` (default
//! `https://app.daytona.io/api`; another host needs a `network` grant),
//! `snapshot`, `organization_id`, `persistent` (default false). Secret:
//! `DAYTONA_API_KEY`.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/credentials", "../../../wit/terminal-backend"],
    world: "prometheus:terminal-backend/terminal-backend",
    generate_all,
});

use exports::prometheus::terminal_backend::plugin::{Command, Execution, Guest, Output, Request};
use prometheus::host::{config, http, secrets, session, state};
use serde_json::{Value, json};

const DEFAULT_API: &str = "https://app.daytona.io/api";
const SECRET: &str = "DAYTONA_API_KEY";
const DEFAULT_CWD: &str = "/home/daytona";
/// How many state reads a starting sandbox gets before it is given up on.
const START_POLLS: u32 = 600;
/// How many follow requests one command may take (each lasts up to the
/// host's request bound, so this is far beyond any command timeout).
const FOLLOWS: u32 = 100_000;

struct Daytona;

fn setting(key: &str) -> Option<Value> {
    config::get(key)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn text_setting(key: &str) -> Option<String> {
    setting(key).and_then(|v| v.as_str().map(str::to_owned))
}

fn persistent() -> bool {
    setting("persistent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn api() -> String {
    text_setting("api_url")
        .unwrap_or_else(|| DEFAULT_API.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

/// The session the call runs for (`default` outside one).
fn session_id() -> String {
    session::current()
        .session_id
        .unwrap_or_else(|| "default".to_owned())
}

/// The state key holding the session's sandbox id.
fn sandbox_key() -> String {
    format!("sandbox:{}", session_id())
}

fn cancel_key(id: &str) -> String {
    format!("cancelled:{id}")
}

/// One API call: the status and the body.
fn call(
    method: &str,
    path: &str,
    body: Option<(String, Vec<u8>)>,
) -> Result<(u16, Vec<u8>), String> {
    let key = secrets::get(SECRET)
        .map_err(|_| format!("{SECRET} is not set (prometheus secrets set {SECRET})"))?;
    let mut headers = vec![("authorization".to_owned(), format!("Bearer {key}"))];
    if let Some(org) = text_setting("organization_id") {
        headers.push(("x-daytona-organization-id".to_owned(), org));
    }
    let body = body.map(|(content_type, bytes)| {
        headers.push(("content-type".to_owned(), content_type));
        bytes
    });
    let response = http::send(&http::Request {
        method: method.to_owned(),
        url: format!("{}{path}", api()),
        headers,
        body,
    })
    .map_err(|e| format!("Daytona API unreachable: {e:?}"))?;
    Ok((response.status, response.body))
}

/// An API call that must succeed, answering its JSON body.
fn json_call(method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let body = body.map(|v| ("application/json".to_owned(), v.to_string().into_bytes()));
    let (status, bytes) = call(method, path, body)?;
    if !(200..300).contains(&status) {
        return Err(failure(method, path, status, &bytes));
    }
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("Daytona {method} {path}: bad JSON: {e}"))
}

fn failure(method: &str, path: &str, status: u16, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let detail = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| text.chars().take(200).collect());
    match status {
        401 | 403 => format!("Daytona rejected {SECRET} ({status}): {detail}"),
        _ => format!("Daytona {method} {path} failed ({status}): {detail}"),
    }
}

/// The session's sandbox, started; created on first use.
fn sandbox() -> Result<String, String> {
    let key = sandbox_key();
    let known = state::get(&key)
        .ok()
        .flatten()
        .and_then(|b| String::from_utf8(b).ok());
    let id = match known {
        Some(id) => id,
        None => {
            let mut body = json!({
                "labels": { "prometheus.session": session_id() },
            });
            if let Some(snapshot) = text_setting("snapshot") {
                body["snapshot"] = Value::String(snapshot);
            }
            let created = json_call("POST", "/sandbox", Some(body))?;
            let id = created
                .get("id")
                .and_then(Value::as_str)
                .ok_or("Daytona created a sandbox without an id")?
                .to_owned();
            state::set(&key, id.as_bytes()).map_err(|e| format!("{e:?}"))?;
            id
        }
    };
    wait_started(&id)?;
    Ok(id)
}

/// Waits until the sandbox runs, starting a stopped one.
fn wait_started(id: &str) -> Result<(), String> {
    let mut asked_start = false;
    for _ in 0..START_POLLS {
        let info = json_call("GET", &format!("/sandbox/{id}"), None)?;
        match info.get("state").and_then(Value::as_str).unwrap_or("") {
            "started" => return Ok(()),
            "stopped" | "archived" if !asked_start => {
                json_call("POST", &format!("/sandbox/{id}/start"), None)?;
                asked_start = true;
            }
            "error" | "build_failed" | "destroyed" => {
                let _ = state::delete(&sandbox_key());
                let reason = info
                    .get("errorReason")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                return Err(format!("Daytona sandbox {id} failed: {reason}"));
            }
            _ => {}
        }
    }
    Err(format!("Daytona sandbox {id} did not start"))
}

fn toolbox(sandbox: &str) -> String {
    format!("/toolbox/{sandbox}/toolbox")
}

/// Runs `request` in its own toolbox session and waits for it.
fn run(sandbox: &str, request: &Request) -> Result<Execution, String> {
    let base = toolbox(sandbox);
    let session = remote_kit::marker(&request.id).to_ascii_lowercase();
    json_call(
        "POST",
        &format!("{base}/process/session"),
        Some(json!({ "sessionId": session })),
    )?;
    let marker = remote_kit::marker(&request.id);
    let script = remote_kit::script(
        &request.line,
        &request.cwd,
        &request.env,
        &marker,
        request.timeout_ms,
    );
    let started = json_call(
        "POST",
        &format!("{base}/process/session/{session}/exec"),
        Some(
            json!({ "command": format!("sh -c {}", remote_kit::quote(&script)), "runAsync": true }),
        ),
    )?;
    let command = started
        .get("cmdId")
        .and_then(Value::as_str)
        .ok_or("Daytona started a command without a cmdId")?
        .to_owned();
    let path = format!("{base}/process/session/{session}/command/{command}");
    let exit_code = wait(&request.id, &path)?;
    let (_, logs) = call("GET", &format!("{path}/logs"), None)?;
    let _ = call("DELETE", &format!("{base}/process/session/{session}"), None);
    let _ = state::delete(&cancel_key(&request.id));
    let (stdout, cwd) = remote_kit::split(&String::from_utf8_lossy(&logs), &marker);
    Ok(Execution {
        exit_code: Some(exit_code),
        stdout,
        stderr: String::new(),
        cwd_observed: cwd,
    })
}

/// The command's exit code once it ended (130 when cancelled first).
fn wait(id: &str, path: &str) -> Result<i32, String> {
    for _ in 0..FOLLOWS {
        if state::get(&cancel_key(id)).ok().flatten().is_some() {
            return Ok(130);
        }
        let info = json_call("GET", path, None)?;
        if let Some(code) = info.get("exitCode").and_then(Value::as_i64) {
            return Ok(i32::try_from(code).unwrap_or(1));
        }
        // Blocks until the command ends (or the request bound elapses).
        let _ = call("GET", &format!("{path}/logs?follow=true"), None);
    }
    Err("the Daytona command never ended".to_owned())
}

impl Guest for Daytona {
    fn run(_: Command) -> Result<Output, String> {
        Err("terminal-backend 1.0 `run` is not supported; the host uses `execute`".to_owned())
    }

    fn name() -> String {
        "daytona".to_owned()
    }

    fn descriptor() -> String {
        json!({
            "isolation": "remote",
            "uses_host_filesystem": false,
            "default_cwd": DEFAULT_CWD,
            "session_isolated_when_ephemeral": true,
            "persistent": persistent(),
        })
        .to_string()
    }

    fn check() -> Result<String, String> {
        // Lists one sandbox: proves the key and the API in one call.
        json_call("GET", "/sandbox?limit=1", None)?;
        Ok(format!("Daytona at {}", api()))
    }

    fn execute(request: Request) -> Result<Execution, String> {
        let sandbox = sandbox()?;
        run(&sandbox, &request)
    }

    fn upload(path: String, contents: Vec<u8>) -> Result<(), String> {
        let sandbox = sandbox()?;
        let (content_type, body) =
            remote_kit::multipart("file", remote_kit::file_name(&path), &contents);
        let target = format!(
            "{}/files/upload?path={}",
            toolbox(&sandbox),
            remote_kit::percent_encode(&path)
        );
        let (status, bytes) = call("POST", &target, Some((content_type, body)))?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(failure("POST", &target, status, &bytes))
        }
    }

    fn download(path: String) -> Result<Vec<u8>, String> {
        let sandbox = sandbox()?;
        let target = format!(
            "{}/files/download?path={}",
            toolbox(&sandbox),
            remote_kit::percent_encode(&path)
        );
        match call("GET", &target, None)? {
            (200..=299, bytes) => Ok(bytes),
            (404, _) => Err(format!("{path}: not found")),
            (status, bytes) => Err(failure("GET", &target, status, &bytes)),
        }
    }

    fn cancel(id: String) {
        let _ = state::set(&cancel_key(&id), b"1");
        if let Some(sandbox) = state::get(&sandbox_key()).ok().flatten() {
            let sandbox = String::from_utf8_lossy(&sandbox);
            let session = remote_kit::marker(&id).to_ascii_lowercase();
            let _ = call(
                "DELETE",
                &format!("{}/process/session/{session}", toolbox(&sandbox)),
                None,
            );
        }
    }

    fn teardown() {
        let key = sandbox_key();
        let Some(sandbox) = state::get(&key).ok().flatten() else {
            return;
        };
        let sandbox = String::from_utf8_lossy(&sandbox).into_owned();
        if persistent() {
            let _ = call("POST", &format!("/sandbox/{sandbox}/stop"), None);
        } else {
            let _ = call("DELETE", &format!("/sandbox/{sandbox}"), None);
            let _ = state::delete(&key);
        }
    }
}

export!(Daytona);
