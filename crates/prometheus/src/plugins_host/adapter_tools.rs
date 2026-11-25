//! Tool calls a catalog adapter plugin runs over its own platform
//! connection (spec: platform-adapters, Feishu and Yuanbao tools;
//! Platform-contributed toolsets). A WASM adapter cannot contribute tools
//! over its world, so its platform's toolset lives in the host
//! (`platform_tools_host`); a tool that needs the adapter's live
//! connection (Yuanbao's group queries ride its WebSocket) reaches the
//! adapter connected for that profile in this process through its `send`
//! with `op = "tool"` and `{name, args}` as the text, and the plugin's
//! answer is the tool's result. Only a plugin whose descriptor declares
//! `tool_ops: true` is registered here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use pmt_plugins::host::{CallInfo, Instance};
use serde_json::{Value, json};

struct Live {
    instance: Arc<Instance>,
    call: CallInfo,
    token: u64,
}

type Registry = HashMap<(PathBuf, String), Live>;

fn registry() -> &'static Mutex<Registry> {
    static LIVE: std::sync::OnceLock<Mutex<Registry>> = std::sync::OnceLock::new();
    LIVE.get_or_init(Mutex::default)
}

/// Records the connected adapter of `platform` for the profile at
/// `profile_dir`; the token [`disconnected`] takes.
pub(crate) fn connected(
    profile_dir: &Path,
    platform: &str,
    instance: Arc<Instance>,
    call: CallInfo,
) -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let token = NEXT.fetch_add(1, Ordering::Relaxed);
    let live = Live {
        instance,
        call,
        token,
    };
    registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert((profile_dir.to_path_buf(), platform.to_owned()), live);
    token
}

/// Forgets the adapter registered with `token` (a newer connection of the
/// same platform is left alone).
pub(crate) fn disconnected(profile_dir: &Path, platform: &str, token: u64) {
    let mut all = registry().lock().unwrap_or_else(PoisonError::into_inner);
    let key = (profile_dir.to_path_buf(), platform.to_owned());
    if all.get(&key).is_some_and(|live| live.token == token) {
        all.remove(&key);
    }
}

/// Runs tool `name` with `args` on the connected `platform` adapter of the
/// profile at `profile_dir`: the plugin's JSON answer, or why it failed
/// (the plugin's failure prefixes removed).
///
/// # Errors
///
/// No such adapter is connected in this process, the plugin refused the
/// call, or the plugin call itself failed.
pub async fn call(
    profile_dir: &Path,
    platform: &str,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    let (instance, info) = {
        let all = registry().lock().unwrap_or_else(PoisonError::into_inner);
        let live = all
            .get(&(profile_dir.to_path_buf(), platform.to_owned()))
            .ok_or_else(|| {
                format!(
                    "the {platform} adapter is not connected in this process; its tools run where the gateway runs it (prometheus gateway status)"
                )
            })?;
        (Arc::clone(&live.instance), live.call.clone())
    };
    let body = json!({
        "op": "tool",
        "chat": args["chat"].as_str().unwrap_or_default(),
        "text": json!({"name": name, "args": args}).to_string(),
    })
    .to_string();
    let answer = pmt_core::spawn::blocking(move || instance.adapter_send(&info, &body))
        .await
        .map_err(|e| format!("plugin call: {e}"))?
        .map_err(|e| e.message().to_owned())?;
    answer.map_err(|why| {
        ["permanent: ", "server: ", "rate_limited: "]
            .iter()
            .find_map(|prefix| why.strip_prefix(prefix))
            .unwrap_or(&why)
            .to_owned()
    })
}
