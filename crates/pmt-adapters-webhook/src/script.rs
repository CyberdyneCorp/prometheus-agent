//! Transform scripts (spec: platform-adapters, Webhook payload filters and
//! transform scripts): a route's `script` runs under the OS sandbox with
//! the payload JSON on stdin. It may read only the profile's `scripts/`
//! directory and the system toolchain, write only its own scratch
//! directory, and has no network. A JSON object on stdout replaces the
//! payload; other output is added as `script_output`; empty output,
//! `[SILENT]`, `{"ignore": true}`, a timeout, a missing file or a non-zero
//! exit drop the event.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_sandbox::{Paths, Policy, Sandbox};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// What the script made of the payload.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The payload to continue with.
    Payload(Value),
    /// Drop the event (why, for the log).
    Ignore(String),
}

/// The capabilities of a transform script: read the scripts and the
/// toolchain, write its scratch, spawn helpers; no network.
const CAPABILITIES: &[&str] = &[
    "fs.read:<workspace>/**",
    "fs.read:<scratch>/**",
    "fs.write:<scratch>/**",
    "fs.read:<toolchain>",
    "process.spawn:*",
];

/// Runs `script` (relative to the profile's `scripts/`) on `payload`.
pub(crate) async fn run(
    scope: &ProfileScope,
    route: &str,
    script: &str,
    timeout: Duration,
    payload: &Value,
) -> Outcome {
    let scripts = scope.path("scripts");
    let file = scripts.join(script);
    // Validation refused traversal; a symlink pointing out is refused here.
    let inside = std::fs::canonicalize(&file)
        .ok()
        .zip(std::fs::canonicalize(&scripts).ok())
        .is_some_and(|(f, root)| f.starts_with(root));
    if !inside || !file.is_file() {
        return Outcome::Ignore(format!("script {} not found", file.display()));
    }
    let scratch = scope.path("cache").join("webhook").join(route);
    if let Err(err) = std::fs::create_dir_all(&scratch) {
        return Outcome::Ignore(format!("scratch {}: {err}", scratch.display()));
    }
    match execute(scope, &scripts, &scratch, &file, timeout, payload).await {
        Ok(stdout) => interpret(&stdout, payload),
        Err(why) => Outcome::Ignore(why),
    }
}

fn sandbox(scope: &ProfileScope, scripts: &Path, scratch: &Path) -> Result<Sandbox, String> {
    let paths = Paths {
        user_home: std::env::home_dir().unwrap_or_else(|| scope.home().to_path_buf()),
        prometheus_home: scope.home().to_path_buf(),
        workspace: scripts.to_path_buf(),
        scratch: vec![scratch.to_path_buf()],
    };
    let policy = Policy::from_capabilities(CAPABILITIES, &paths).map_err(|e| e.to_string())?;
    Sandbox::new(policy, &scope.config().security.sandbox).map_err(|e| e.to_string())
}

async fn execute(
    scope: &ProfileScope,
    scripts: &Path,
    scratch: &PathBuf,
    file: &Path,
    timeout: Duration,
    payload: &Value,
) -> Result<String, String> {
    let sandbox = sandbox(scope, scripts, scratch)?;
    let mut command = sandbox.command(file).map_err(|e| e.to_string())?;
    command
        .current_dir(scratch)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", scratch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("script {} did not start: {e}", file.display()))?;
    let input = payload.to_string();
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let work = async move {
        // A script that never reads stdin must not block the run.
        let feed = async move {
            let _ = stdin.write_all(input.as_bytes()).await;
        };
        let mut out = Vec::new();
        let read = stdout.read_to_end(&mut out);
        let ((), read) = tokio::join!(feed, read);
        read.map_err(|e| e.to_string())?;
        let status = child.wait().await.map_err(|e| e.to_string())?;
        if status.success() {
            Ok(String::from_utf8_lossy(&out).into_owned())
        } else {
            Err(format!("script exited with {status}"))
        }
    };
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| format!("script timed out after {}s", timeout.as_secs()))?
}

/// What stdout means for the event.
fn interpret(stdout: &str, payload: &Value) -> Outcome {
    let out = stdout.trim();
    if out.is_empty() || out == "[SILENT]" {
        return Outcome::Ignore("script produced no output".to_owned());
    }
    match serde_json::from_str::<Value>(out) {
        Ok(Value::Object(map)) if map.get("ignore") == Some(&Value::Bool(true)) => {
            Outcome::Ignore("script asked to ignore the event".to_owned())
        }
        Ok(object @ Value::Object(_)) => Outcome::Payload(object),
        _ => {
            let mut next = match payload {
                Value::Object(_) => payload.clone(),
                other => serde_json::json!({ "payload": other }),
            };
            next["script_output"] = Value::String(out.to_owned());
            Outcome::Payload(next)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn output_decides_the_payload() {
        let payload = json!({"a": 1});
        assert!(matches!(interpret("", &payload), Outcome::Ignore(_)));
        assert!(matches!(
            interpret(" [SILENT]\n", &payload),
            Outcome::Ignore(_)
        ));
        assert!(matches!(
            interpret("{\"ignore\": true}", &payload),
            Outcome::Ignore(_)
        ));
        assert_eq!(
            interpret("{\"b\": 2}", &payload),
            Outcome::Payload(json!({"b": 2}))
        );
        assert_eq!(
            interpret("deploy failed\n", &payload),
            Outcome::Payload(json!({"a": 1, "script_output": "deploy failed"}))
        );
    }
}
