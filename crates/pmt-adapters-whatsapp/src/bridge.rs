//! The `WhatsApp` Web bridge: a loopback HTTP service that holds the linked
//! device session and speaks the multi-device protocol. The adapter polls
//! `GET /messages` (the bridge queues inbound messages until they are
//! fetched, so a reconnect loses nothing), sends with `POST /send`,
//! `/edit`, `/send-media` and `/typing`, reads `GET /chat/<id>` and
//! watches `GET /health`. Without `options.bridge_url` the adapter starts
//! the managed bridge runtime (`<home>/runtimes/whatsapp-bridge/`, run by
//! the managed Node runtime) on a free loopback port.
//!
//! Loopback is reachable by every local user and process, so each request
//! carries a bearer token (`Authorization: Bearer`): a fresh random one per
//! managed launch, handed to the child in `PMT_BRIDGE_TOKEN`; for a bridge
//! at `options.bridge_url`, the secret named by `token_secret`
//! ([`TOKEN_SECRET`] by default). The managed bridge is third-party npm
//! code: it starts with a cleared environment, so the daemon's provider
//! keys never reach it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_gateway::messaging::{AdapterError, SendError};
use serde_json::Value;

use crate::PLATFORM;

/// A bridge client.
#[derive(Clone)]
pub(crate) struct Bridge {
    pub(crate) client: reqwest::Client,
    /// `http://127.0.0.1:<port>`, no trailing slash.
    pub(crate) url: String,
    /// The bearer token every request carries.
    pub(crate) token: String,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

const CALL_TIMEOUT: Duration = Duration::from_mins(1);

fn transport(err: reqwest::Error) -> SendError {
    if err.is_timeout() {
        SendError::Ambiguous(err.without_url().to_string())
    } else {
        SendError::Network(err.without_url().to_string())
    }
}

impl Bridge {
    /// `GET` a bridge path.
    pub(crate) async fn get(&self, path: &str) -> Result<Value, SendError> {
        let sent = self
            .client
            .get(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .timeout(CALL_TIMEOUT)
            .send()
            .await;
        answer(sent).await
    }

    /// `POST` JSON to a bridge path.
    pub(crate) async fn post(&self, path: &str, body: &Value) -> Result<Value, SendError> {
        let sent = self
            .client
            .post(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(body.to_string())
            .timeout(CALL_TIMEOUT)
            .send()
            .await;
        answer(sent).await
    }
}

async fn answer(sent: Result<reqwest::Response, reqwest::Error>) -> Result<Value, SendError> {
    let response = sent.map_err(transport)?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok());
    let text = response.text().await.map_err(transport)?;
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(body);
    }
    let error = body["error"].as_str().unwrap_or("request failed");
    Err(refusal(status, error, retry_after))
}

/// The send error of a bridge refusal.
pub(crate) fn refusal(status: u16, error: &str, retry_after: Option<u64>) -> SendError {
    match status {
        429 => SendError::RateLimited {
            retry_after: retry_after.map(Duration::from_secs),
        },
        // 503: the bridge lost WhatsApp for now.
        500.. => SendError::Server(format!("{status} {error}")),
        _ => SendError::Permanent(format!("{status} {error}")),
    }
}

/// What the bridge's health says about the linked device.
pub(crate) fn health_error(health: &Value, profile: &str) -> Option<AdapterError> {
    match health["status"].as_str().unwrap_or_default() {
        "connected" => None,
        "logged_out" => Some(AdapterError {
            retryable: false,
            code: "whatsapp_logged_out".to_owned(),
            message: format!(
                "WhatsApp logged this device out; link it again with `prometheus -p {profile} gateway whatsapp pair`"
            ),
        }),
        other => Some(AdapterError {
            retryable: true,
            code: "whatsapp_disconnected".to_owned(),
            message: format!("the WhatsApp bridge is not connected ({other})"),
        }),
    }
}

/// Where the managed bridge runtime lives.
pub(crate) fn script(home: &Path) -> PathBuf {
    crate::install::dir(home).join("bridge.js")
}

/// The secret holding the token of a bridge at `options.bridge_url`.
pub const TOKEN_SECRET: &str = "WHATSAPP_BRIDGE_TOKEN";
/// The environment variable the bridge reads its token from.
pub const TOKEN_ENV: &str = "PMT_BRIDGE_TOKEN";
/// The only variables of the daemon's environment the managed bridge
/// inherits: what Node needs to run and find a temporary directory.
const KEPT_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "TMPDIR",
    "LANG",
];

/// The bridge's per-profile places: the linked-device session under
/// `<profile>/platforms/whatsapp/session/` and the caches inbound media
/// land in under `<profile>/cache/media/whatsapp/`, so each profile's media
/// stay inside its own directory (and its media retention).
#[must_use]
pub fn profile_args(profile_dir: &Path) -> Vec<OsString> {
    let session = profile_dir.join("platforms").join(PLATFORM).join("session");
    let cache = profile_dir.join("cache").join("media").join(PLATFORM);
    let mut args = vec![OsString::from("--session"), session.into_os_string()];
    for kind in ["image", "document", "audio"] {
        args.push(format!("--{kind}-cache").into());
        args.push(cache.join(kind).into_os_string());
    }
    args
}

/// Clears `command`'s environment down to [`KEPT_ENV`]: the bridge is
/// third-party npm code and must not see the daemon's secrets.
pub fn isolate(command: &mut tokio::process::Command) {
    command.env_clear().envs(
        KEPT_ENV
            .iter()
            .filter_map(|k| Some((k, std::env::var_os(k)?))),
    );
}

/// A fresh random bearer token (256 bits, hex).
pub(crate) fn new_token() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random source works");
    pmt_gateway::messaging::sign::hex(&bytes)
}

/// The command that runs `script` with `node` for a profile: cleared
/// environment plus [`KEPT_ENV`] and the token.
pub(crate) fn command(
    node: &Path,
    script: &Path,
    profile_dir: &Path,
    port: u16,
    mode: &str,
    token: &str,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(node);
    isolate(&mut command);
    command
        .env(TOKEN_ENV, token)
        .arg(script)
        .arg("--port")
        .arg(port.to_string())
        .args(profile_args(profile_dir))
        .arg("--mode")
        .arg(mode);
    command
}

/// Starts the managed bridge for a profile on a free loopback port: the
/// child (it dies with the handle), its URL and its token.
pub(crate) fn launch(
    home: &Path,
    profile_dir: &Path,
    profile: &str,
    mode: &str,
) -> Result<(tokio::process::Child, String, String), AdapterError> {
    let missing = || AdapterError {
        retryable: false,
        code: "whatsapp_bridge_missing".to_owned(),
        message: format!(
            "the WhatsApp bridge runtime is not installed or is outdated; run `prometheus -p {profile} gateway whatsapp pair`"
        ),
    };
    let script = script(home);
    // An older installed bridge would ignore the token and the caches.
    let current = crate::install::refresh(home).map_err(|e| AdapterError {
        retryable: true,
        code: "whatsapp_bridge_failed".to_owned(),
        message: e.message().to_owned(),
    })?;
    if !current || !script.is_file() {
        return Err(missing());
    }
    let node = pmt_update::runtimes::Store::open(home)
        .installed("node")
        .ok_or_else(missing)?
        .exe();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map_err(|e| AdapterError {
            retryable: true,
            code: "whatsapp_bridge_failed".to_owned(),
            message: format!("no loopback port for the bridge: {e}"),
        })?
        .port();
    let token = new_token();
    let child = command(&node, &script, profile_dir, port, mode, &token)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| AdapterError {
            retryable: true,
            code: "whatsapp_bridge_failed".to_owned(),
            message: format!("cannot start the WhatsApp bridge: {e}"),
        })?;
    Ok((child, format!("http://127.0.0.1:{port}"), token))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The managed bridge gets profile-scoped cache directories, a token,
    /// and none of the daemon's other environment (provider keys).
    #[cfg(unix)]
    #[tokio::test]
    async fn the_managed_bridge_gets_profile_caches_a_token_and_a_clean_environment() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles").join("work");
        let out = dir.path().join("seen");
        let script = dir.path().join("bridge.sh");
        std::fs::write(
            &script,
            format!("echo \"$@\" > '{0}.args'\nenv > '{0}.env'\n", out.display()),
        )
        .unwrap();
        let status = command(Path::new("/bin/sh"), &script, &profile, 4242, "bot", "t0k")
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let args = std::fs::read_to_string(out.with_extension("args")).unwrap();
        let cache = profile.join("cache/media/whatsapp");
        for kind in ["image", "document", "audio"] {
            let expected = format!("--{kind}-cache {}", cache.join(kind).display());
            assert!(args.contains(&expected), "{args}");
        }
        assert!(
            args.contains("--port 4242") && args.contains("--mode bot"),
            "{args}"
        );
        assert!(!args.contains(".hermes"), "{args}");
        let env = std::fs::read_to_string(out.with_extension("env")).unwrap();
        assert!(env.lines().any(|l| l == "PMT_BRIDGE_TOKEN=t0k"), "{env}");
        for line in env.lines() {
            let name = line.split('=').next().unwrap_or_default();
            // sh itself sets a few (PWD, SHLVL, _).
            let shell = matches!(name, "PWD" | "SHLVL" | "_" | "OLDPWD");
            assert!(
                shell || name == TOKEN_ENV || KEPT_ENV.contains(&name),
                "inherited {name}"
            );
        }
    }

    #[test]
    fn tokens_are_fresh_and_long() {
        let (a, b) = (new_token(), new_token());
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn refusals_and_health_map_onto_errors() {
        assert_eq!(
            refusal(429, "slow down", Some(7)),
            SendError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        assert!(matches!(refusal(503, "x", None), SendError::Server(_)));
        assert!(matches!(refusal(400, "x", None), SendError::Permanent(_)));
        assert_eq!(health_error(&json!({"status": "connected"}), "work"), None);
        let out = health_error(&json!({"status": "logged_out"}), "work").unwrap();
        assert_eq!(out.code, "whatsapp_logged_out");
        assert!(!out.retryable);
        assert!(
            out.message
                .contains("prometheus -p work gateway whatsapp pair")
        );
        assert!(
            health_error(&json!({"status": "connecting"}), "work")
                .unwrap()
                .retryable
        );
    }
}
