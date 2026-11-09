//! `managed` mode: the Nous subscription's Modal gateway, a REST API over
//! the host's `credentials.send`, which adds the subscription token (usage
//! is billed to the subscription).
//!
//! `POST /v1/sandboxes` creates the session's sandbox (idempotent per
//! session), `POST .../execs` starts a command (answering its final result
//! at once when it already ended) and `GET .../execs/<id>` polls it until a
//! final status; `POST .../execs/<id>/cancel` stops one and
//! `POST .../terminate` ends the sandbox. The gateway merges stdout and
//! stderr into `output`. Files travel through commands: an upload is
//! base64 on the command's stdin, a download base64 in its output.

use exports::prometheus::terminal_backend::plugin::{Execution, Request};
use prometheus::credentials::handles;
use serde_json::{Value, json};

use crate::{
    DEFAULT_CWD, HANDLE, Sandbox, cancelled, exports, failure_text, image, number_setting,
    prometheus, script, session_id,
};

/// Polls before a command is given up on.
const POLLS: u32 = 1_000_000;
const FINAL: &[&str] = &["completed", "failed", "cancelled", "timeout"];

fn call(
    origin: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    extra: &[(&str, String)],
) -> Result<(u16, Value), String> {
    let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
    headers.extend(extra.iter().map(|(k, v)| ((*k).to_owned(), v.clone())));
    let request = handles::Request {
        method: method.to_owned(),
        url: format!("{origin}{path}"),
        headers,
        body: body.map(|b| b.to_string().into_bytes()),
    };
    let response = handles::send(HANDLE, &request)
        .map_err(|f| format!("Modal gateway {method} {path}: {}", failure_text(&f)))?;
    let value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    Ok((response.status, value))
}

fn ok(method: &str, path: &str, (status, value): (u16, Value)) -> Result<Value, String> {
    if (200..300).contains(&status) {
        return Ok(value);
    }
    let detail = ["error", "message", "code"]
        .iter()
        .find_map(|k| value.get(k).and_then(Value::as_str))
        .map_or_else(|| value.to_string(), str::to_owned);
    Err(format!(
        "Modal gateway {method} {path} failed ({status}): {detail}"
    ))
}

pub(crate) fn create(origin: &str) -> Result<Sandbox, String> {
    let session = session_id();
    let body = json!({
        "image": image(),
        "cwd": DEFAULT_CWD,
        "cpu": 1,
        "memoryMiB": 5120,
        "timeoutMs": number_setting("timeout_secs", 3600) * 1000,
        "idleTimeoutMs": number_setting("idle_timeout_secs", 900) * 1000,
        "persistentFilesystem": false,
        "logicalKey": session,
    });
    let idempotency = [("x-idempotency-key", format!("prometheus-sandbox-{session}"))];
    let created = ok(
        "POST",
        "/v1/sandboxes",
        call(origin, "POST", "/v1/sandboxes", Some(body), &idempotency)?,
    )?;
    let sandbox = created
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("the Modal gateway created a sandbox without an id")?;
    Ok(Sandbox::Managed {
        sandbox: sandbox.to_owned(),
        origin: origin.to_owned(),
    })
}

/// The final `(returncode, output)` of exec `id`, or none while it runs.
fn finished(body: &Value) -> Option<(i32, String)> {
    let status = body.get("status").and_then(Value::as_str)?;
    FINAL.contains(&status).then(|| {
        let code = body
            .get("returncode")
            .and_then(Value::as_i64)
            .and_then(|c| i32::try_from(c).ok())
            .unwrap_or(1);
        let output = body
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        (code, output)
    })
}

/// Runs `command` (with `stdin` when given) and waits for its result.
fn run(
    origin: &str,
    sandbox: &str,
    exec: &str,
    command: &str,
    stdin: Option<String>,
    timeout_ms: u64,
) -> Result<(i32, String), String> {
    let path = format!("/v1/sandboxes/{sandbox}/execs");
    let mut body = json!({
        "execId": exec,
        "command": command,
        "cwd": DEFAULT_CWD,
    });
    if timeout_ms != u64::MAX {
        body["timeoutMs"] = json!(timeout_ms);
    }
    if let Some(stdin) = stdin {
        body["stdinData"] = json!(stdin);
    }
    let started = ok("POST", &path, call(origin, "POST", &path, Some(body), &[])?)?;
    if let Some(done) = finished(&started) {
        return Ok(done);
    }
    let poll = format!("{path}/{exec}");
    for _ in 0..POLLS {
        if cancelled(exec) {
            return Ok((130, String::new()));
        }
        let (status, body) = call(origin, "GET", &poll, None, &[])?;
        if status == 404 {
            return Err(format!("Modal gateway exec {exec}: 404 not found"));
        }
        if let Some(done) = finished(&ok("GET", &poll, (status, body))?) {
            return Ok(done);
        }
    }
    Err("the Modal command never ended".to_owned())
}

pub(crate) fn execute(origin: &str, sandbox: &str, request: &Request) -> Result<Execution, String> {
    let (script, marker) = script(request);
    let command = format!("sh -c {}", remote_kit::quote(&script));
    let (code, output) = run(
        origin,
        sandbox,
        &request.id,
        &command,
        None,
        request.timeout_ms,
    )?;
    let (stdout, cwd) = remote_kit::split(&output, &marker);
    Ok(Execution {
        exit_code: Some(code),
        stdout,
        stderr: String::new(),
        cwd_observed: cwd,
    })
}

/// A fresh exec id for a file transfer (the gateway keys execs by id).
fn file_exec_id(verb: &str) -> String {
    let key = "file-exec-seq";
    let n = prometheus::host::state::get(key)
        .ok()
        .flatten()
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    let _ = prometheus::host::state::set(key, n.to_string().as_bytes());
    format!("{verb}-{}-{n}", session_id())
}

pub(crate) fn upload(
    origin: &str,
    sandbox: &str,
    path: &str,
    contents: &[u8],
) -> Result<(), String> {
    let quoted = remote_kit::quote(path);
    let command = format!("mkdir -p \"$(dirname {quoted})\" && base64 -d > {quoted}");
    let exec = file_exec_id("upload");
    let (code, output) = run(
        origin,
        sandbox,
        &exec,
        &command,
        Some(remote_kit::base64_encode(contents)),
        u64::MAX,
    )?;
    if code == 0 {
        Ok(())
    } else {
        Err(format!("upload of {path} failed: {}", output.trim()))
    }
}

pub(crate) fn download(origin: &str, sandbox: &str, path: &str) -> Result<Vec<u8>, String> {
    let quoted = remote_kit::quote(path);
    let command =
        format!("if [ -f {quoted} ]; then base64 < {quoted}; else echo missing; exit 44; fi");
    let exec = file_exec_id("download");
    let (code, output) = run(origin, sandbox, &exec, &command, None, u64::MAX)?;
    match code {
        0 => remote_kit::base64_decode(&output)
            .ok_or_else(|| format!("download of {path}: the gateway returned no base64")),
        44 => Err(format!("{path}: not found")),
        _ => Err(format!("download of {path} failed: {}", output.trim())),
    }
}

pub(crate) fn cancel(origin: &str, sandbox: &str, id: &str) {
    let path = format!("/v1/sandboxes/{sandbox}/execs/{id}/cancel");
    let _ = call(origin, "POST", &path, None, &[]);
}

pub(crate) fn terminate(origin: &str, sandbox: &str) {
    let path = format!("/v1/sandboxes/{sandbox}/terminate");
    let _ = call(
        origin,
        "POST",
        &path,
        Some(json!({"snapshotBeforeTerminate": false})),
        &[],
    );
}
