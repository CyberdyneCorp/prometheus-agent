//! First-party `terminal-backend` plugin for Modal sandboxes (spec:
//! execution-environments, First-party backend plugins; plugin-system,
//! Host credential handles).
//!
//! The plugin never holds a Modal credential: every request goes through
//! the host's `credentials.send` with the handle `modal`, and the host
//! attaches the secret for the mode `terminal.modal.mode` resolves to.
//! `describe("modal")` tells the plugin which mode that is:
//!
//! - `direct`: the user's Modal account, Modal's gRPC API at its origin
//!   (see [`direct`]).
//! - `managed`: the Nous subscription's Modal gateway, a REST API billed to
//!   the subscription (see [`managed`]).
//!
//! One sandbox per session, created on first use and kept in the plugin's
//! state store (with the mode it was made in) so every instance finds it;
//! `teardown` terminates it. Settings (`plugins.entries.modal.*`): `image`
//! (default `python:3.12-slim`), `app_name` (direct, default
//! `prometheus`), `environment` (direct), `timeout_secs` (sandbox lifetime,
//! default 3600), `idle_timeout_secs` (default 900).

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/credentials", "../../../wit/terminal-backend"],
    world: "prometheus:terminal-backend/terminal-backend",
    generate_all,
});

mod direct;
mod managed;
mod proto;

use exports::prometheus::terminal_backend::plugin::{Command, Execution, Guest, Output, Request};
use prometheus::credentials::handles::{self, Failure};
use prometheus::host::{config, session, state};
use serde_json::{Value, json};

/// The host credential the plugin sends every request with.
pub(crate) const HANDLE: &str = "modal";
pub(crate) const DEFAULT_CWD: &str = "/root";
const DEFAULT_IMAGE: &str = "python:3.12-slim";

struct Modal;

fn setting(key: &str) -> Option<Value> {
    config::get(key)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
}

pub(crate) fn text_setting(key: &str) -> Option<String> {
    setting(key).and_then(|v| v.as_str().map(str::to_owned))
}

pub(crate) fn number_setting(key: &str, default: u64) -> u64 {
    setting(key).and_then(|v| v.as_u64()).unwrap_or(default)
}

pub(crate) fn image() -> String {
    text_setting("image").unwrap_or_else(|| DEFAULT_IMAGE.to_owned())
}

/// The session the call runs for (`default` outside one).
pub(crate) fn session_id() -> String {
    session::current()
        .session_id
        .unwrap_or_else(|| "default".to_owned())
}

fn sandbox_key() -> String {
    format!("sandbox:{}", session_id())
}

pub(crate) fn cancel_key(id: &str) -> String {
    format!("cancelled:{id}")
}

pub(crate) fn cancelled(id: &str) -> bool {
    state::get(&cancel_key(id)).ok().flatten().is_some()
}

pub(crate) fn failure_text(failure: &Failure) -> String {
    match failure {
        Failure::Denied(why)
        | Failure::UnknownHandle(why)
        | Failure::Unavailable(why)
        | Failure::HostNotAllowed(why)
        | Failure::Failed(why) => why.clone(),
    }
}

/// The credential's mode and origin.
fn describe() -> Result<handles::Info, String> {
    handles::describe(HANDLE).map_err(|f| failure_text(&f))
}

/// The session's sandbox, as stored.
#[derive(Clone)]
pub(crate) enum Sandbox {
    Direct {
        sandbox: String,
        task: String,
        origin: String,
    },
    Managed {
        sandbox: String,
        origin: String,
    },
}

impl Sandbox {
    fn load() -> Option<Self> {
        let bytes = state::get(&sandbox_key()).ok().flatten()?;
        let v: Value = serde_json::from_slice(&bytes).ok()?;
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        match v.get("mode").and_then(Value::as_str)? {
            "direct" => Some(Self::Direct {
                sandbox: text("sandbox")?,
                task: text("task")?,
                origin: text("origin")?,
            }),
            "managed" => Some(Self::Managed {
                sandbox: text("sandbox")?,
                origin: text("origin")?,
            }),
            _ => None,
        }
    }

    fn store(&self) -> Result<(), String> {
        let v = match self {
            Self::Direct {
                sandbox,
                task,
                origin,
            } => json!({"mode": "direct", "sandbox": sandbox, "task": task, "origin": origin}),
            Self::Managed { sandbox, origin } => {
                json!({"mode": "managed", "sandbox": sandbox, "origin": origin})
            }
        };
        state::set(&sandbox_key(), v.to_string().as_bytes()).map_err(|e| format!("{e:?}"))
    }

    fn forget() {
        let _ = state::delete(&sandbox_key());
    }

    /// The session's sandbox, created on first use in the mode the
    /// credential resolves to now.
    fn get() -> Result<Self, String> {
        if let Some(known) = Self::load() {
            return Ok(known);
        }
        let info = describe()?;
        let sandbox = match info.kind.as_str() {
            "direct" => direct::create(&info.origin)?,
            "managed" => managed::create(&info.origin)?,
            other => return Err(format!("unknown Modal credential mode '{other}'")),
        };
        sandbox.store()?;
        Ok(sandbox)
    }
}

/// Whether a failure means the sandbox is gone (ended at its timeout or
/// idle timeout), so a new one should be made.
pub(crate) fn sandbox_gone(why: &str) -> bool {
    why.contains("NOT_FOUND") || why.contains("terminated") || why.contains("404")
}

fn with_sandbox<T>(run: impl Fn(&Sandbox) -> Result<T, String>) -> Result<T, String> {
    let sandbox = Sandbox::get()?;
    match run(&sandbox) {
        Err(why) if sandbox_gone(&why) => {
            Sandbox::forget();
            run(&Sandbox::get()?)
        }
        other => other,
    }
}

/// The script `request` runs as, and its marker.
pub(crate) fn script(request: &Request) -> (String, String) {
    let marker = remote_kit::marker(&request.id);
    let script = remote_kit::script(
        &request.line,
        &request.cwd,
        &request.env,
        &marker,
        request.timeout_ms,
    );
    (script, marker)
}

impl Guest for Modal {
    fn run(_: Command) -> Result<Output, String> {
        Err("terminal-backend 1.0 `run` is not supported; the host uses `execute`".to_owned())
    }

    fn name() -> String {
        "modal".to_owned()
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
        let info = describe()?;
        Ok(match info.billing_source {
            Some(billing) => format!(
                "mode: {} at {} (billing_source = {billing})",
                info.kind, info.origin
            ),
            None => format!("mode: {} at {}", info.kind, info.origin),
        })
    }

    fn execute(request: Request) -> Result<Execution, String> {
        let result = with_sandbox(|sandbox| match sandbox {
            Sandbox::Direct { task, origin, .. } => direct::execute(origin, task, &request),
            Sandbox::Managed { sandbox, origin } => managed::execute(origin, sandbox, &request),
        });
        let _ = state::delete(&cancel_key(&request.id));
        result
    }

    fn upload(path: String, contents: Vec<u8>) -> Result<(), String> {
        with_sandbox(|sandbox| match sandbox {
            Sandbox::Direct { task, origin, .. } => direct::upload(origin, task, &path, &contents),
            Sandbox::Managed { sandbox, origin } => {
                managed::upload(origin, sandbox, &path, &contents)
            }
        })
    }

    fn download(path: String) -> Result<Vec<u8>, String> {
        with_sandbox(|sandbox| match sandbox {
            Sandbox::Direct { task, origin, .. } => direct::download(origin, task, &path),
            Sandbox::Managed { sandbox, origin } => managed::download(origin, sandbox, &path),
        })
    }

    fn cancel(id: String) {
        let _ = state::set(&cancel_key(&id), b"1");
        match Sandbox::load() {
            Some(Sandbox::Direct { task, origin, .. }) => direct::cancel(&origin, &task, &id),
            Some(Sandbox::Managed { sandbox, origin }) => managed::cancel(&origin, &sandbox, &id),
            None => {}
        }
    }

    fn teardown() {
        let Some(sandbox) = Sandbox::load() else {
            return;
        };
        match &sandbox {
            Sandbox::Direct {
                sandbox, origin, ..
            } => direct::terminate(origin, sandbox),
            Sandbox::Managed { sandbox, origin } => managed::terminate(origin, sandbox),
        }
        Sandbox::forget();
    }
}

export!(Modal);
