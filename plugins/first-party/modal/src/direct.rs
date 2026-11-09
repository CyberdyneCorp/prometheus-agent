//! `direct` mode: Modal's gRPC API (`modal.client.ModalClient`) over the
//! host's `credentials.send`, which adds the user's token headers.
//!
//! Unary calls only need what plain HTTP gives: a reply message in the
//! body, or a failed status in the headers (gRPC "trailers-only"
//! responses), so no trailers are read; a reply with neither is an error.
//! Server-streaming calls (image build logs, command output) are read as
//! one bounded response each and repeated from where they stopped.
//!
//! A sandbox runs an idle loop from the configured image (built once per
//! app with `FROM <image>`); each command is a `ContainerExec` of the
//! shared remote script, waited on with `ContainerExecWait` long-polls so
//! `cancel` on another instance is noticed, then its stdout and stderr are
//! read.

use exports::prometheus::terminal_backend::plugin::{Execution, Request};
use prometheus::credentials::handles;
use prometheus::host::state;

use crate::proto::{Fields, Msg, frame, percent_decode, unframe};
use crate::{
    DEFAULT_CWD, HANDLE, Sandbox, cancelled, exports, failure_text, image, number_setting,
    prometheus, script, text_setting,
};

const SERVICE: &str = "/modal.client.ModalClient/";
/// The image builder version Modal's SDKs default to.
const BUILDER_VERSION: &str = "2024.10";
/// Seconds one long-poll may wait server-side (well inside the host's
/// per-request bound).
const POLL_SECS: f32 = 10.0;
/// Long-polls before a wait is given up on (each up to [`POLL_SECS`]).
const POLLS: u32 = 100_000;
/// Bytes per stdin message of an upload.
const CHUNK: usize = 512 * 1024;
/// `EXEC_OUTPUT_OPTION_PIPE`.
const PIPE: u64 = 2;
const STDOUT: u64 = 1;
const STDERR: u64 = 2;
/// `OBJECT_CREATION_TYPE_CREATE_IF_MISSING`.
const CREATE_IF_MISSING: u64 = 1;

fn code_name(code: &str) -> &str {
    match code {
        "1" => "CANCELLED",
        "3" => "INVALID_ARGUMENT",
        "4" => "DEADLINE_EXCEEDED",
        "5" => "NOT_FOUND",
        "7" => "PERMISSION_DENIED",
        "8" => "RESOURCE_EXHAUSTED",
        "9" => "FAILED_PRECONDITION",
        "13" => "INTERNAL",
        "14" => "UNAVAILABLE",
        "16" => "UNAUTHENTICATED",
        other => other,
    }
}

/// One call: every reply message.
fn call(origin: &str, method: &str, message: Msg) -> Result<Vec<Fields>, String> {
    let header = |k: &str, v: &str| (k.to_owned(), v.to_owned());
    let request = handles::Request {
        method: "POST".to_owned(),
        url: format!("{origin}{SERVICE}{method}"),
        headers: vec![
            header("content-type", "application/grpc"),
            header("te", "trailers"),
            header("grpc-accept-encoding", "identity"),
            header("x-modal-client-type", "1"),
            header("x-modal-client-version", "1.0.0"),
        ],
        body: Some(frame(&message.finish())),
    };
    let response = handles::send(HANDLE, &request)
        .map_err(|f| format!("Modal {method}: {}", failure_text(&f)))?;
    let find = |name: &str| {
        response
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    if let Some(status) = find("grpc-status")
        && status != "0"
    {
        let detail = find("grpc-message")
            .map(|m| percent_decode(&m))
            .unwrap_or_default();
        return Err(format!(
            "Modal {method} failed: {} {detail}",
            code_name(&status)
        ));
    }
    if response.status != 200 {
        return Err(format!("Modal {method} failed: HTTP {}", response.status));
    }
    let messages = unframe(&response.body).map_err(|e| format!("Modal {method}: {e}"))?;
    if messages.is_empty() {
        return Err(format!(
            "Modal {method}: no reply message (its status was only in the trailers)"
        ));
    }
    messages.iter().map(|m| Fields::parse(m)).collect()
}

fn unary(origin: &str, method: &str, message: Msg) -> Result<Fields, String> {
    call(origin, method, message)?
        .into_iter()
        .next()
        .ok_or_else(|| format!("Modal {method}: empty reply"))
}

fn cached(key: &str, make: impl FnOnce() -> Result<String, String>) -> Result<String, String> {
    if let Some(value) = state::get(key)
        .ok()
        .flatten()
        .and_then(|b| String::from_utf8(b).ok())
    {
        return Ok(value);
    }
    let value = make()?;
    let _ = state::set(key, value.as_bytes());
    Ok(value)
}

/// `GenericResult.status` names.
fn result_failure(result: &Fields) -> String {
    let status = match result.uint(1) {
        2 => "failed",
        3 => "terminated",
        4 => "timed out",
        5 => "failed to start",
        6 => "failed internally",
        7 => "idle timeout",
        _ => "ended",
    };
    let exception = result.text(2);
    if exception.is_empty() {
        status.to_owned()
    } else {
        format!("{status}: {exception}")
    }
}

fn app_id(origin: &str) -> Result<String, String> {
    let name = text_setting("app_name").unwrap_or_else(|| "prometheus".to_owned());
    let environment = text_setting("environment").unwrap_or_default();
    cached(&format!("app:{origin}:{environment}:{name}"), || {
        let mut message = Msg::new().text(1, &name);
        if !environment.is_empty() {
            message = message.text(2, &environment);
        }
        let reply = unary(origin, "AppGetOrCreate", message.uint(3, CREATE_IF_MISSING))?;
        Some(reply.text(1))
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "Modal AppGetOrCreate returned no app id".to_owned())
    })
}

fn image_id(origin: &str, app: &str) -> Result<String, String> {
    let reference = image();
    cached(&format!("image:{origin}:{app}:{reference}"), || {
        let definition = Msg::new().texts(6, &[format!("FROM {reference}")]);
        let reply = unary(
            origin,
            "ImageGetOrCreate",
            Msg::new()
                .message(2, definition)
                .text(4, app)
                .text(9, BUILDER_VERSION),
        )?;
        let id = reply.text(1);
        let mut result = reply.message(2).filter(|r| r.uint(1) != 0);
        let mut last = String::new();
        for _ in 0..POLLS {
            if result.is_some() {
                break;
            }
            let replies = call(
                origin,
                "ImageJoinStreaming",
                Msg::new()
                    .text(1, &id)
                    .float(2, POLL_SECS)
                    .text(3, &last)
                    .flag(4, true),
            )?;
            for reply in replies {
                let entry = reply.text(3);
                if !entry.is_empty() {
                    last = entry;
                }
                if let Some(done) = reply.message(1).filter(|r| r.uint(1) != 0) {
                    result = Some(done);
                }
            }
        }
        match result {
            Some(r) if r.uint(1) == 1 => Ok(id),
            Some(r) => Err(format!(
                "Modal image {reference} build {}",
                result_failure(&r)
            )),
            None => Err(format!("Modal image {reference} build never finished")),
        }
    })
}

/// A new sandbox from the configured image, ready to run commands.
pub(crate) fn create(origin: &str) -> Result<Sandbox, String> {
    let app = app_id(origin)?;
    let image = image_id(origin, &app)?;
    let definition = Msg::new()
        .texts(1, &["sh", "-c", "while :; do sleep 3600; done"])
        .text(3, &image)
        .uint(7, number_setting("timeout_secs", 3600))
        .text(8, DEFAULT_CWD)
        .uint(33, number_setting("idle_timeout_secs", 900));
    let reply = unary(
        origin,
        "SandboxCreate",
        Msg::new().text(1, &app).message(2, definition),
    )?;
    let sandbox = reply.text(1);
    if sandbox.is_empty() {
        return Err("Modal SandboxCreate returned no sandbox id".to_owned());
    }
    for _ in 0..POLLS {
        let reply = unary(
            origin,
            "SandboxGetTaskId",
            Msg::new()
                .text(1, &sandbox)
                .float(2, POLL_SECS)
                .flag(3, true),
        )?;
        if let Some(result) = reply.message(2).filter(|r| r.uint(1) != 0) {
            return Err(format!(
                "Modal sandbox {sandbox} {}",
                result_failure(&result)
            ));
        }
        let task = reply.text(1);
        if !task.is_empty() {
            return Ok(Sandbox::Direct {
                sandbox,
                task,
                origin: origin.to_owned(),
            });
        }
    }
    Err(format!("Modal sandbox {sandbox} never started"))
}

/// What one exec did.
struct Ran {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `argv` in the task, feeding `stdin` when given, and waits for it
/// (exit 130 as soon as request `id` is cancelled).
fn run(
    origin: &str,
    task: &str,
    argv: &[&str],
    stdin: Option<&[u8]>,
    id: Option<&str>,
) -> Result<Ran, String> {
    let reply = unary(
        origin,
        "ContainerExec",
        Msg::new()
            .text(1, task)
            .texts(2, argv)
            .uint(6, PIPE)
            .uint(7, PIPE),
    )?;
    let exec = reply.text(1);
    if exec.is_empty() {
        return Err("Modal ContainerExec returned no exec id".to_owned());
    }
    if let Some(input) = stdin {
        put_input(origin, &exec, input)?;
    }
    let mut exit_code = None;
    let mut completed = false;
    for _ in 0..POLLS {
        if id.is_some_and(cancelled) {
            return Ok(Ran {
                exit_code: Some(130),
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        let reply = unary(
            origin,
            "ContainerExecWait",
            Msg::new().text(1, &exec).float(2, POLL_SECS),
        )?;
        if reply.flag(2) {
            exit_code = reply.int(1);
            completed = true;
            break;
        }
    }
    if !completed {
        return Err("the Modal command never ended".to_owned());
    }
    Ok(Ran {
        exit_code,
        stdout: output(origin, &exec, STDOUT)?,
        stderr: output(origin, &exec, STDERR)?,
    })
}

fn put_input(origin: &str, exec: &str, input: &[u8]) -> Result<(), String> {
    let mut index = 0_u64;
    for chunk in input.chunks(CHUNK) {
        index += 1;
        let message = Msg::new().bytes(1, chunk).uint(2, index);
        unary(
            origin,
            "ContainerExecPutInput",
            Msg::new().text(1, exec).message(2, message),
        )?;
    }
    let end = Msg::new().uint(2, index + 1).flag(3, true);
    unary(
        origin,
        "ContainerExecPutInput",
        Msg::new().text(1, exec).message(2, end),
    )?;
    Ok(())
}

/// Everything the exec wrote to `fd`, read until the batch that carries
/// its exit code.
fn output(origin: &str, exec: &str, fd: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut index = 0_u64;
    for _ in 0..POLLS {
        let batches = call(
            origin,
            "ContainerExecGetOutput",
            Msg::new()
                .text(1, exec)
                .float(2, POLL_SECS)
                .uint(3, index)
                .uint(4, fd)
                .flag(5, true),
        )?;
        let mut done = false;
        for batch in batches {
            for item in batch.messages(1) {
                let bytes = item.bytes(3);
                if bytes.is_empty() {
                    out.extend_from_slice(item.text(2).as_bytes());
                } else {
                    out.extend_from_slice(&bytes);
                }
            }
            index = index.max(batch.uint(2));
            done |= batch.has(3);
        }
        if done {
            return Ok(out);
        }
    }
    Err("the Modal command output never ended".to_owned())
}

pub(crate) fn execute(origin: &str, task: &str, request: &Request) -> Result<Execution, String> {
    let (script, marker) = script(request);
    let ran = run(
        origin,
        task,
        &["sh", "-c", &script],
        None,
        Some(&request.id),
    )?;
    let (stdout, cwd) = remote_kit::split(&String::from_utf8_lossy(&ran.stdout), &marker);
    Ok(Execution {
        exit_code: ran.exit_code,
        stdout,
        stderr: String::from_utf8_lossy(&ran.stderr).into_owned(),
        cwd_observed: cwd,
    })
}

pub(crate) fn upload(origin: &str, task: &str, path: &str, contents: &[u8]) -> Result<(), String> {
    let script = "mkdir -p \"$(dirname \"$1\")\" && cat > \"$1\"";
    let ran = run(
        origin,
        task,
        &["sh", "-c", script, "sh", path],
        Some(contents),
        None,
    )?;
    if ran.exit_code == Some(0) {
        Ok(())
    } else {
        Err(format!(
            "upload of {path} failed: {}",
            String::from_utf8_lossy(&ran.stderr).trim()
        ))
    }
}

pub(crate) fn download(origin: &str, task: &str, path: &str) -> Result<Vec<u8>, String> {
    let ran = run(origin, task, &["cat", "--", path], None, None)?;
    match ran.exit_code {
        Some(0) => Ok(ran.stdout),
        _ if !ran.stderr.is_empty()
            && String::from_utf8_lossy(&ran.stderr).contains("No such file") =>
        {
            Err(format!("{path}: not found"))
        }
        _ => Err(format!(
            "download of {path} failed: {}",
            String::from_utf8_lossy(&ran.stderr).trim()
        )),
    }
}

/// Kills the processes running request `id`'s script (found by its
/// marker; slim images have no `pkill`).
pub(crate) fn cancel(origin: &str, task: &str, id: &str) {
    let marker = remote_kit::marker(id);
    let script = format!(
        "for p in /proc/[0-9]*; do n=${{p#/proc/}}; [ \"$n\" = \"$$\" ] && continue; \
         grep -q {marker} \"$p/cmdline\" 2>/dev/null && kill -9 \"$n\"; done; true"
    );
    let _ = unary(
        origin,
        "ContainerExec",
        Msg::new()
            .text(1, task)
            .texts(2, &["sh", "-c", &script])
            .uint(6, PIPE)
            .uint(7, PIPE),
    );
}

pub(crate) fn terminate(origin: &str, sandbox: &str) {
    let _ = unary(origin, "SandboxTerminate", Msg::new().text(1, sandbox));
}
