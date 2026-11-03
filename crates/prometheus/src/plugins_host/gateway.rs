//! The messaging gateway's events reach the profile's plugin hooks (spec:
//! plugin-system, Typed hook registry; Streaming and lifecycle observer
//! hooks: `post_gateway_admission`; Gateway hook directories): the
//! gateway's hook registry calls one bridge per profile, which fires the
//! profile's current hook set (plugin callbacks, then shell hooks, hook
//! directories and outbound webhooks) with surface `gateway`.
//! `pre_command` answers map onto the gateway's decision: `block` denies
//! with its message, `context` rewrites the command line, `modify` is read
//! as the gateway's own JSON answer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_gateway::messaging::{Hook, HookBridge};
use pmt_plugins::hooks::Payload;
use pmt_plugins::host::CallInfo;
use pmt_plugins::host::hook_world::Reply;
use serde_json::{Value, json};

/// How often a busy gateway re-checks a profile's plugins, hooks and
/// grants (consent and config changes apply within this bound).
pub const REFRESH: Duration = Duration::from_secs(10);

/// Finds a profile's scope.
pub type Scopes = Arc<dyn Fn(&str) -> Option<ProfileScope> + Send + Sync>;

/// The bridge of every profile the gateway serves.
pub struct Bridge {
    scopes: Scopes,
}

impl Bridge {
    /// A bridge resolving profiles through `scopes`.
    #[must_use]
    pub fn new(scopes: Scopes) -> Self {
        Self { scopes }
    }
}

impl HookBridge for Bridge {
    fn hook(&self, profile: &str) -> Option<Arc<dyn Hook>> {
        let scope = (self.scopes)(profile)?;
        Some(Arc::new(ProfileHook { scope }))
    }
}

struct ProfileHook {
    scope: ProfileScope,
}

impl Hook for ProfileHook {
    fn call(
        &self,
        name: &str,
        payload: &Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, Error>> + Send + '_>>
    {
        let name = name.to_owned();
        let payload = payload.clone();
        Box::pin(async move {
            let plugins = super::current(&self.scope, REFRESH);
            let hooks = plugins.current().hooks.clone();
            if !hooks.handles(&name) {
                return Ok(Value::Null);
            }
            let fields = match payload {
                Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            let call = CallInfo {
                session_id: fields
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                surface: "gateway".to_owned(),
                cwd: None,
            };
            let answers = hooks.fire(&name, &Payload::new(fields), &call).await;
            if name != "pre_command" {
                return Ok(Value::Null);
            }
            Ok(answers
                .into_iter()
                .find_map(|answer| pre_command(answer.reply))
                .unwrap_or(Value::Null))
        })
    }
}

/// A `pre_command` reply as the gateway's decision (`None`: continue).
fn pre_command(reply: Reply) -> Option<Value> {
    match reply {
        Reply::Block(message) => Some(json!({"action": "deny", "message": message})),
        Reply::Context(text) => Some(json!({"action": "rewrite", "text": text})),
        Reply::Modify(text) => serde_json::from_str(&text).ok(),
        Reply::None | Reply::Approve(_) => None,
    }
}

/// When a profile's set was last checked (for [`super::current`]).
pub(super) fn due(last: &mut Option<Instant>, every: Duration) -> bool {
    let now = Instant::now();
    match last {
        Some(at) if now.duration_since(*at) < every => false,
        _ => {
            *last = Some(now);
            true
        }
    }
}
