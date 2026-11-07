//! First-party `terminal-backend` plugin for Vercel Sandbox (spec:
//! execution-environments, First-party backend plugins; Remote sandboxes
//! are plugins).
//!
//! One sandbox (a microVM) per session, created on first use and kept in
//! the plugin's state store so every instance finds it; one that stopped
//! (Vercel ends a sandbox at its timeout) is replaced by a new one.
//! `teardown` stops it. Vercel sandboxes are ephemeral: the descriptor
//! says so.
//!
//! A command runs detached (`POST .../cmd`) and is waited on with the
//! long-polling `?wait=true` read, repeated when the host's request bound
//! ends one; its id is kept in state so `cancel` (on another instance)
//! can kill it. Output comes from the command's NDJSON log, split into
//! stdout and stderr. Files go up as a gzip tar (`fs/write`) and come
//! back through `fs/read`.
//!
//! Settings (`plugins.entries.vercel.*`): `team_id` and `project_id`
//! (required), `runtime` (default `node22`), `vcpus` (default 2),
//! `timeout_ms` (the sandbox's lifetime, default 45 minutes), `api_url`
//! (default `https://api.vercel.com`). Secret: `VERCEL_TOKEN`.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/terminal-backend"],
    world: "prometheus:terminal-backend/terminal-backend",
    generate_all,
});

use exports::prometheus::terminal_backend::plugin::{Command, Execution, Guest, Output, Request};
use prometheus::host::{config, http, secrets, session, state};
use serde_json::{Value, json};

const DEFAULT_API: &str = "https://api.vercel.com";
const SECRET: &str = "VERCEL_TOKEN";
const DEFAULT_CWD: &str = "/vercel/sandbox";
const DEFAULT_LIFETIME_MS: u64 = 45 * 60 * 1000;
/// How many status reads a pending sandbox gets.
const START_POLLS: u32 = 600;
/// How many long-polling waits one command may take.
const WAITS: u32 = 100_000;

struct Vercel;

fn setting(key: &str) -> Option<Value> {
    config::get(key)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn text_setting(key: &str) -> Option<String> {
    setting(key).and_then(|v| v.as_str().map(str::to_owned))
}

fn api() -> String {
    text_setting("api_url")
        .unwrap_or_else(|| DEFAULT_API.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

fn required(key: &str) -> Result<String, String> {
    text_setting(key).ok_or_else(|| {
        format!("plugins.entries.vercel.{key} is not set (prometheus config set plugins.entries.vercel.{key} '\"...\"')")
    })
}

fn session_id() -> String {
    session::current()
        .session_id
        .unwrap_or_else(|| "default".to_owned())
}

fn sandbox_key() -> String {
    format!("sandbox:{}", session_id())
}

fn command_key(id: &str) -> String {
    format!("command:{id}")
}

fn cancel_key(id: &str) -> String {
    format!("cancelled:{id}")
}

fn read_state(key: &str) -> Option<String> {
    state::get(key)
        .ok()
        .flatten()
        .and_then(|b| String::from_utf8(b).ok())
}

/// One API call (`teamId` added to the query): the status and the body.
fn call(method: &str, path: &str, body: Option<(&str, Vec<u8>)>) -> Result<(u16, Vec<u8>), String> {
    let token = secrets::get(SECRET)
        .map_err(|_| format!("{SECRET} is not set (prometheus secrets set {SECRET})"))?;
    let team = required("team_id")?;
    let mut headers = vec![("authorization".to_owned(), format!("Bearer {token}"))];
    let body = body.map(|(content_type, bytes)| {
        headers.push(("content-type".to_owned(), content_type.to_owned()));
        if content_type == "application/gzip" {
            headers.push(("x-cwd".to_owned(), "/".to_owned()));
        }
        bytes
    });
    let join = if path.contains('?') { '&' } else { '?' };
    let response = http::send(&http::Request {
        method: method.to_owned(),
        url: format!(
            "{}{path}{join}teamId={}",
            api(),
            remote_kit::percent_encode(&team)
        ),
        headers,
        body,
    })
    .map_err(|e| format!("Vercel API unreachable: {e:?}"))?;
    Ok((response.status, response.body))
}

/// An API call that must succeed, answering its JSON body.
fn json_call(method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let body = body.map(|v| ("application/json", v.to_string().into_bytes()));
    let (status, bytes) = call(method, path, body)?;
    if !(200..300).contains(&status) {
        return Err(failure(method, path, status, &bytes));
    }
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("Vercel {method} {path}: bad JSON: {e}"))
}

fn failure(method: &str, path: &str, status: u16, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let detail = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| text.chars().take(200).collect());
    match status {
        401 | 403 => format!("Vercel rejected {SECRET} ({status}): {detail}"),
        _ => format!("Vercel {method} {path} failed ({status}): {detail}"),
    }
}

fn status_of(sandbox: &Value) -> &str {
    sandbox
        .pointer("/sandbox/status")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// The session's running sandbox; a new one when there is none or it
/// stopped.
fn sandbox() -> Result<String, String> {
    if let Some(id) = read_state(&sandbox_key()) {
        let (status, bytes) = call("GET", &format!("/v1/sandboxes/{id}"), None)?;
        let info: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status == 200 && matches!(status_of(&info), "running" | "pending") {
            wait_running(&id)?;
            return Ok(id);
        }
    }
    let created = json_call("POST", "/v1/sandboxes", Some(create_body()?))?;
    let id = created
        .pointer("/sandbox/id")
        .and_then(Value::as_str)
        .ok_or("Vercel created a sandbox without an id")?
        .to_owned();
    state::set(&sandbox_key(), id.as_bytes()).map_err(|e| format!("{e:?}"))?;
    wait_running(&id)?;
    Ok(id)
}

fn create_body() -> Result<Value, String> {
    Ok(json!({
        "projectId": required("project_id")?,
        "runtime": text_setting("runtime").unwrap_or_else(|| "node22".to_owned()),
        "resources": { "vcpus": setting("vcpus").and_then(|v| v.as_u64()).unwrap_or(2) },
        "timeout": setting("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_LIFETIME_MS),
        "ports": [],
    }))
}

fn wait_running(id: &str) -> Result<(), String> {
    for _ in 0..START_POLLS {
        let info = json_call("GET", &format!("/v1/sandboxes/{id}"), None)?;
        match status_of(&info) {
            "running" => return Ok(()),
            "pending" => {}
            other => {
                let _ = state::delete(&sandbox_key());
                return Err(format!("Vercel sandbox {id} is {other}"));
            }
        }
    }
    Err(format!("Vercel sandbox {id} did not start"))
}

/// Starts `request` and waits for it.
fn run(sandbox: &str, request: &Request) -> Result<Execution, String> {
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
        &format!("/v1/sandboxes/{sandbox}/cmd"),
        Some(json!({ "command": "sh", "args": ["-c", script], "cwd": DEFAULT_CWD, "env": {} })),
    )?;
    let command = started
        .pointer("/command/id")
        .and_then(Value::as_str)
        .ok_or("Vercel started a command without an id")?
        .to_owned();
    let _ = state::set(&command_key(&request.id), command.as_bytes());
    let path = format!("/v1/sandboxes/{sandbox}/cmd/{command}");
    let exit_code = wait(&request.id, &path);
    let _ = state::delete(&command_key(&request.id));
    let _ = state::delete(&cancel_key(&request.id));
    let exit_code = exit_code?;
    let (stdout, stderr) = logs(&format!("{path}/logs"))?;
    let (stdout, cwd) = remote_kit::split(&stdout, &marker);
    Ok(Execution {
        exit_code: Some(exit_code),
        stdout,
        stderr,
        cwd_observed: cwd,
    })
}

/// The command's exit code once it ended (130 when cancelled first).
fn wait(id: &str, path: &str) -> Result<i32, String> {
    for _ in 0..WAITS {
        if state::get(&cancel_key(id)).ok().flatten().is_some() {
            // A cancel that came before the command id was kept could not
            // kill it itself.
            let _ = call(
                "POST",
                &format!("{path}/kill"),
                Some(("application/json", br#"{"signal":15}"#.to_vec())),
            );
            return Ok(130);
        }
        // Long-polls until the command ends; a request the host's bound
        // ended is simply asked again.
        let info = match json_call("GET", &format!("{path}?wait=true"), None) {
            Ok(info) => info,
            Err(e) if e.contains("timed out") => continue,
            Err(e) => return Err(e),
        };
        if let Some(code) = info.pointer("/command/exitCode").and_then(Value::as_i64) {
            return Ok(i32::try_from(code).unwrap_or(1));
        }
    }
    Err("the Vercel command never ended".to_owned())
}

/// stdout and stderr from the command's NDJSON log.
fn logs(path: &str) -> Result<(String, String), String> {
    let (status, bytes) = call("GET", path, None)?;
    if !(200..300).contains(&status) {
        return Err(failure("GET", path, status, &bytes));
    }
    let (mut stdout, mut stderr) = (String::new(), String::new());
    for line in String::from_utf8_lossy(&bytes).lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let data = entry.get("data").and_then(Value::as_str).unwrap_or("");
        match entry.get("stream").and_then(Value::as_str) {
            Some("stderr") => stderr.push_str(data),
            _ => stdout.push_str(data),
        }
    }
    Ok((stdout, stderr))
}

impl Guest for Vercel {
    fn run(_: Command) -> Result<Output, String> {
        Err("terminal-backend 1.0 `run` is not supported; the host uses `execute`".to_owned())
    }

    fn name() -> String {
        "vercel".to_owned()
    }

    fn descriptor() -> String {
        json!({
            "isolation": "remote",
            "uses_host_filesystem": false,
            "default_cwd": DEFAULT_CWD,
            "session_isolated_when_ephemeral": true,
            "persistent": false,
        })
        .to_string()
    }

    fn check() -> Result<String, String> {
        required("project_id")?;
        json_call("GET", "/v1/sandboxes?limit=1", None)?;
        Ok(format!("Vercel Sandbox at {}", api()))
    }

    fn execute(request: Request) -> Result<Execution, String> {
        let sandbox = sandbox()?;
        run(&sandbox, &request)
    }

    fn upload(path: String, contents: Vec<u8>) -> Result<(), String> {
        let sandbox = sandbox()?;
        let target = format!("/v1/sandboxes/{sandbox}/fs/write");
        let body = remote_kit::tar_gz(&path, &contents);
        let (status, bytes) = call("POST", &target, Some(("application/gzip", body)))?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(failure("POST", &target, status, &bytes))
        }
    }

    fn download(path: String) -> Result<Vec<u8>, String> {
        let sandbox = sandbox()?;
        let target = format!("/v1/sandboxes/{sandbox}/fs/read");
        let body = json!({ "path": path }).to_string().into_bytes();
        match call("POST", &target, Some(("application/json", body)))? {
            (200..=299, bytes) => Ok(bytes),
            (404, _) => Err(format!("{path}: not found")),
            (status, bytes) => Err(failure("POST", &target, status, &bytes)),
        }
    }

    fn cancel(id: String) {
        let _ = state::set(&cancel_key(&id), b"1");
        if let (Some(sandbox), Some(command)) =
            (read_state(&sandbox_key()), read_state(&command_key(&id)))
        {
            let path = format!("/v1/sandboxes/{sandbox}/cmd/{command}/kill");
            let _ = call(
                "POST",
                &path,
                Some(("application/json", br#"{"signal":15}"#.to_vec())),
            );
        }
    }

    fn teardown() {
        if let Some(sandbox) = read_state(&sandbox_key()) {
            let _ = call("POST", &format!("/v1/sandboxes/{sandbox}/stop"), None);
            let _ = state::delete(&sandbox_key());
        }
    }
}

export!(Vercel);
