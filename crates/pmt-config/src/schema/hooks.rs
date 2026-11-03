//! The `[hooks]` section (spec: plugin-system, Shell hooks with first-use
//! consent; Outbound webhooks): shell hooks under `[[hooks.<event>]]`,
//! outbound webhooks under `[[hooks.outbound]]`, and whether unapproved
//! shell hooks run without asking.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[hooks]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
#[schemars(extend("x-apply" = "next_session"))]
pub struct HooksConfig {
    /// Run shell hooks nobody approved yet without asking (as
    /// `--accept-hooks` does for one run); every hook still runs in the
    /// session's sandbox.
    pub auto_accept: bool,
    /// Outbound webhooks: hook events sent as HTTPS `POST` requests.
    pub outbound: Vec<OutboundHook>,
    /// Shell hooks by event (`[[hooks.pre_tool_call]]`...); the event is a
    /// name of the closed hook registry.
    #[serde(flatten)]
    pub events: BTreeMap<String, Vec<ShellHook>>,
}

/// One `[[hooks.<event>]]` shell hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellHook {
    /// The program and its arguments, split like a shell would but run
    /// without one; the event's JSON arrives on stdin.
    pub command: String,
    /// A regular expression the tool name must match (tool events only).
    #[serde(default)]
    pub matcher: Option<String>,
    /// How long one run may take (clamped to 300).
    #[serde(default = "default_shell_timeout")]
    #[schemars(range(min = 1))]
    pub timeout_secs: u64,
    /// `pre_tool_call` only: a hook that fails (cannot start, times out,
    /// answers garbage) blocks the call instead of letting it run.
    #[serde(default)]
    pub fail_closed: bool,
}

const fn default_shell_timeout() -> u64 {
    60
}

/// One `[[hooks.outbound]]` webhook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutboundHook {
    /// Unique name (logs and `prometheus plugins hooks list`).
    pub name: String,
    /// Where events are sent (`POST`): `https://`, or `http://` to a loopback
    /// address.
    pub url: String,
    /// The hook events delivered (names of the closed hook registry).
    pub events: Vec<String>,
    /// A regular expression the tool name must match (tool events only).
    #[serde(default)]
    pub matcher: Option<String>,
    /// The secrets-store entry whose value signs each delivery
    /// (`X-Prometheus-Signature-256`); unsigned without one.
    #[serde(default)]
    pub secret_name: Option<String>,
    /// How long one delivery attempt may take.
    #[serde(default = "default_outbound_timeout")]
    #[schemars(range(min = 1, max = 60))]
    pub timeout_secs: u64,
}

const fn default_outbound_timeout() -> u64 {
    10
}

impl ShellHook {
    /// The run bound: `timeout_secs` clamped to 300 seconds.
    #[must_use]
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs.clamp(1, 300))
    }
}
