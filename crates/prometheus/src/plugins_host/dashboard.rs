//! Dashboard plugin contributions on the daemon side (spec: web-dashboard,
//! Dashboard plugin contributions): what the profile's plugins declare
//! (`pmt_plugins::dashboard`), the HTML their frames load, and their
//! backend routes.
//!
//! A backend request never runs in the daemon process: the daemon starts
//! the plugin host, `prometheus plugins panel-call <id>` for the
//! profile, which loads the profile's plugins with their grants, passes
//! the request JSON to the plugin's `ui-panel` `on-action` and prints the
//! answer. A plugin that traps or hangs takes only that process down
//! (killed after [`CALL_TIMEOUT`]).

use std::io::Read as _;
use std::path::Path;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_plugins::dashboard::Contributions;
use pmt_plugins::host::CallInfo;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;

use crate::context::Context;

/// The largest frame document served.
const MAX_FRAME: u64 = 1024 * 1024;
/// The largest request or answer of a backend call.
const MAX_CALL: usize = 16 * 1024 * 1024;
/// How long one backend call may take, plugin host start included.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The dashboard contributions of the plugins `scope`'s discovery finds
/// now.
#[must_use]
pub fn contributions(scope: &ProfileScope) -> Contributions {
    let version = super::running_version();
    let blocked = super::catalog::kill_list(scope);
    let inputs = super::inputs(scope, scope.dir(), &version, &blocked);
    pmt_plugins::dashboard::contributions(&pmt_plugins::load::survey(&inputs))
}

fn not_found(id: &str) -> Error {
    Error::new(
        &codes::PLUGINS_NOT_FOUND,
        format!("no enabled plugin '{id}' contributes to the dashboard"),
    )
}

/// The HTML document of plugin `id`'s frames.
///
/// # Errors
///
/// `plugins.not_found`; `plugins.load` for a missing or oversized entry.
pub fn frame(scope: &ProfileScope, id: &str) -> Result<String, Error> {
    let found = contributions(scope);
    let plugin = found.get(id).ok_or_else(|| not_found(id))?;
    let entry = &plugin.entry;
    let len = std::fs::metadata(entry)
        .map_err(|e| Error::io(&codes::PLUGINS_LOAD, entry, &e))?
        .len();
    if len > MAX_FRAME {
        return Err(Error::new(
            &codes::PLUGINS_LOAD,
            format!("{} is larger than 1 MiB", entry.display()),
        ));
    }
    std::fs::read_to_string(entry).map_err(|e| Error::io(&codes::PLUGINS_LOAD, entry, &e))
}

/// Runs backend `request` of plugin `id` in a plugin host process.
///
/// # Errors
///
/// `plugins.not_found` for a plugin without a backend; `plugins.call`
/// when the plugin host fails, times out or answers badly.
pub async fn call(scope: &ProfileScope, id: &str, request: &Value) -> Result<Value, Error> {
    let found = contributions(scope);
    if !found.get(id).is_some_and(|p| p.backend) {
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("plugin '{id}' serves no dashboard backend routes"),
        ));
    }
    let exe = std::env::current_exe()
        .map_err(|e| Error::new(&codes::PLUGINS_CALL, format!("no plugin host binary: {e}")))?;
    let mut child = tokio::process::Command::new(exe)
        .arg("--home")
        .arg(scope.home())
        .args(["-p", scope.name().as_str(), "plugins", "panel-call", id])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| host_failed(id, &e.to_string()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| host_failed(id, "no stdin"))?;
    let body = request.to_string();
    let exchange = async {
        stdin
            .write_all(body.as_bytes())
            .await
            .map_err(|e| host_failed(id, &e.to_string()))?;
        drop(stdin);
        child
            .wait_with_output()
            .await
            .map_err(|e| host_failed(id, &e.to_string()))
    };
    let output = tokio::time::timeout(CALL_TIMEOUT, exchange)
        .await
        .map_err(|_| host_failed(id, "no answer within 30 s"))??;
    if output.stdout.len() > MAX_CALL {
        return Err(host_failed(id, "the answer is larger than 16 MiB"));
    }
    serde_json::from_slice(&output.stdout).map_err(|_| {
        host_failed(
            id,
            &format!("exit {:?} without an answer", output.status.code()),
        )
    })
}

fn host_failed(id: &str, why: &str) -> Error {
    Error::new(
        &codes::PLUGINS_CALL,
        format!("the plugin host for '{id}' failed: {why}"),
    )
}

/// The answer `on-action` gave, as `{"status", "body"}`: an object with a
/// numeric `status` is taken as is, anything else is a 200 body; the
/// plugin's own error is a 400.
fn answer(outcome: Result<String, String>) -> Value {
    match outcome {
        Ok(text) => {
            let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
            if parsed.get("status").and_then(Value::as_u64).is_some() {
                json!({"status": parsed["status"], "body": parsed.get("body").cloned().unwrap_or(Value::Null)})
            } else {
                json!({"status": 200, "body": parsed})
            }
        }
        Err(message) => json!({"status": 400, "body": {"error": "plugin", "message": message}}),
    }
}

/// `prometheus plugins panel-call <id>`: the plugin host side of one
/// backend request (the request JSON on stdin, the answer on stdout).
///
/// # Errors
///
/// `plugins.not_found` when the plugin is not loaded; `plugins.call` when
/// it traps or times out.
pub fn serve_call(ctx: &Context<'_>, id: &str) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let mut request = String::new();
    std::io::stdin()
        .take(MAX_CALL as u64)
        .read_to_string(&mut request)
        .map_err(|e| {
            Error::new(
                &codes::PLUGINS_CALL,
                format!("cannot read the request: {e}"),
            )
        })?;
    let answered = respond(&scope, scope.dir(), id, &request)?;
    crate::output::print_raw(&answered.to_string())?;
    Ok(0)
}

fn respond(scope: &ProfileScope, cwd: &Path, id: &str, request: &str) -> Result<Value, Error> {
    let loaded = super::for_session(scope, cwd).current();
    let plugin = loaded
        .plugins
        .iter()
        .find(|p| p.manifest.id == id)
        .ok_or_else(|| not_found(id))?;
    let call = CallInfo {
        session_id: None,
        surface: "dashboard".to_owned(),
        cwd: None,
    };
    Ok(answer(plugin.instance.panel_action(&call, request)?))
}
