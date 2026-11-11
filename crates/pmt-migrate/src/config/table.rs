//! The declarative mapping of Hermes `config.yaml` keys to Prometheus
//! `config.toml` keys (spec: hermes-migration, Config translation from
//! user-set values only).
//!
//! A key listed in [`ROWS`] maps to its target through its transform. Any
//! other Hermes key whose dotted name is also a Prometheus key (the two
//! schemas share most section and key names) maps unchanged when its value
//! is a scalar or a list of scalars, and the Prometheus schema accepts it.
//! Everything else is unsupported. Keys another component owns are listed
//! in [`OWNED_ELSEWHERE`].

use crate::component::Component;
use crate::host::Registry;

/// How a Hermes value becomes the Prometheus one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transform {
    /// Unchanged.
    Same,
    /// A number multiplied by the factor (a unit change), as an integer.
    Scale(f64),
    /// A string lower-cased.
    Lower,
    /// A name checked against a registry (see [`covered_by_default`]).
    Name(Registry),
    /// A list of names translated through a registry.
    Names(Registry),
}

/// One explicit row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Row {
    /// The dotted Hermes key.
    pub hermes: &'static str,
    /// The dotted Prometheus key.
    pub target: &'static str,
    /// The value transform.
    pub transform: Transform,
}

const fn row(hermes: &'static str, target: &'static str, transform: Transform) -> Row {
    Row {
        hermes,
        target,
        transform,
    }
}

const fn secs(hermes: &'static str, target: &'static str) -> Row {
    row(hermes, target, Transform::Scale(1.0))
}

const fn ms(hermes: &'static str, target: &'static str) -> Row {
    row(hermes, target, Transform::Scale(1000.0))
}

/// Keys that are renamed, rescaled or translated.
pub const ROWS: &[Row] = &[
    row("max_turns", "agent.max_iterations", Transform::Same),
    row("agent.max_turns", "agent.max_iterations", Transform::Same),
    row("model", "model.default", Transform::Same),
    row("model.default", "model.default", Transform::Same),
    row(
        "model.provider",
        "model.provider",
        Transform::Name(Registry::Provider),
    ),
    row(
        "display.skin",
        "display.theme",
        Transform::Name(Registry::Theme),
    ),
    row(
        "display.busy_input_mode",
        "busy.input_mode",
        Transform::Same,
    ),
    row("timezone", "cron.timezone", Transform::Same),
    row(
        "toolsets",
        "tools.enabled_toolsets",
        Transform::Names(Registry::Toolset),
    ),
    row(
        "agent.disabled_toolsets",
        "tools.disabled_toolsets",
        Transform::Names(Registry::Toolset),
    ),
    row("logging.level", "logging.level", Transform::Lower),
    row(
        "cron.provider",
        "cron.provider",
        Transform::Name(Registry::Provider),
    ),
    row(
        "cron.model_provider",
        "cron.model_provider",
        Transform::Name(Registry::Provider),
    ),
    row(
        "streaming.enabled",
        "gateway.streaming.enabled",
        Transform::Same,
    ),
    row(
        "streaming.buffer_threshold",
        "gateway.streaming.buffer_threshold",
        Transform::Same,
    ),
    row(
        "streaming.cursor",
        "gateway.streaming.cursor",
        Transform::Same,
    ),
    row(
        "streaming.transport",
        "gateway.streaming.transport",
        Transform::Same,
    ),
    ms(
        "streaming.edit_interval",
        "gateway.streaming.edit_interval_ms",
    ),
    secs("agent.clarify_timeout", "agent.clarify_timeout_secs"),
    secs("agent.fast_auto_seconds", "agent.fast_auto_secs"),
    secs("agent.run_budget_seconds", "agent.run_budget_secs"),
    secs(
        "agent.session_stall_timeout",
        "gateway.session_stall_timeout_secs",
    ),
    secs(
        "agent.reconnect_attention_after",
        "gateway.reconnect_attention_secs",
    ),
    secs(
        "agent.agent_cache.idle_ttl_secs",
        "gateway.agent_cache.idle_ttl_secs",
    ),
    row(
        "agent.agent_cache.max_size",
        "gateway.agent_cache.max_size",
        Transform::Same,
    ),
    secs("approvals.timeout", "approvals.timeout_secs"),
    secs("browser.dialog_timeout_s", "browser.dialog_timeout_secs"),
    secs(
        "browser.inactivity_timeout",
        "browser.inactivity_timeout_secs",
    ),
    secs(
        "code_execution.kernel_idle_timeout",
        "code_execution.kernel_idle_timeout_secs",
    ),
    secs(
        "compression.idle_compact_after_seconds",
        "compression.idle_compact_after_secs",
    ),
    secs("cron.script_timeout_seconds", "cron.script_timeout_secs"),
    row(
        "cron.failure_repeat_alert_hours",
        "cron.failure_repeat_alert_secs",
        Transform::Scale(3600.0),
    ),
    secs("loops.min_interval_seconds", "loops.min_interval_secs"),
    secs(
        "loops.self_paced_ceiling_seconds",
        "loops.self_paced_ceiling_secs",
    ),
    secs(
        "loops.self_paced_floor_seconds",
        "loops.self_paced_floor_secs",
    ),
    secs("lsp.broken_retry_seconds", "lsp.broken_retry_secs"),
    secs("lsp.idle_timeout", "lsp.idle_timeout_secs"),
    ms("lsp.wait_timeout", "lsp.wait_timeout_ms"),
    ms("lsp.warmup_timeout", "lsp.warmup_timeout_ms"),
    ms("plugins.load_timeout_seconds", "plugins.load_timeout_ms"),
    ms(
        "skills.inline_shell_timeout",
        "skills.inline_shell_timeout_ms",
    ),
    secs(
        "stt.local.unload_after_idle_seconds",
        "stt.local.unload_after_idle_secs",
    ),
    secs("stt.openai.timeout", "stt.openai.timeout_secs"),
    secs("terminal.timeout", "terminal.timeout_secs"),
    secs("tts.keep_warm_seconds", "tts.keep_warm_secs"),
    secs("voice.max_recording_seconds", "voice.max_recording_secs"),
    ms("voice.silence_duration", "voice.silence_duration_ms"),
    secs("x_search.timeout_seconds", "x_search.timeout_secs"),
    row(
        "gateway.max_inbound_media_bytes",
        "gateway.media.max_inbound_bytes",
        Transform::Same,
    ),
];

/// Whether a Hermes name maps to nothing because the Prometheus default
/// covers it: the `hermes-<surface>` toolset presets select each
/// surface's default tools. Other names are taken as they are.
#[must_use]
pub fn covered_by_default(registry: Registry, name: &str) -> bool {
    registry == Registry::Toolset && name.starts_with("hermes-")
}

/// Hermes keys (or key prefixes ending in `.`) another component reports.
pub const OWNED_ELSEWHERE: &[(&str, Component)] = &[
    ("mcp_servers", Component::Mcp),
    ("platforms", Component::Gateway),
    ("gateway.platforms", Component::Gateway),
    ("skills.disabled", Component::Skills),
    ("skills.platform_disabled", Component::Skills),
    ("plugins.enabled", Component::Plugins),
    ("plugins.disabled", Component::Plugins),
    ("personalities", Component::Identity),
    ("agent.personalities", Component::Identity),
    ("agent.personality", Component::Identity),
    ("display.personality", Component::Identity),
    ("display.language", Component::Identity),
    ("agent.system_prompt", Component::Identity),
    ("credential_pool_strategies", Component::Secrets),
    ("memory.provider", Component::Memory),
];

/// The component that reports Hermes key `key` when it is not the config
/// component.
#[must_use]
pub fn owner(key: &str) -> Option<Component> {
    OWNED_ELSEWHERE
        .iter()
        .find(|(prefix, _)| key == *prefix || key.starts_with(&format!("{prefix}.")))
        .map(|(_, c)| *c)
}

/// The explicit row of `key`.
#[must_use]
pub fn row_of(key: &str) -> Option<&'static Row> {
    ROWS.iter().find(|r| r.hermes == key)
}

/// Keys whose meaning makes an unclear value a security question: an
/// ambiguous value takes the most restrictive Prometheus value.
#[must_use]
pub fn security_sensitive(key: &str) -> bool {
    const PREFIXES: &[&str] = &["approvals.", "security.", "privacy.", "network."];
    const WORDS: &[&str] = &["sandbox", "redact", "allow_private", "trust", "egress"];
    PREFIXES.iter().any(|p| key.starts_with(p)) || WORDS.iter().any(|w| key.contains(w))
}

/// The most restrictive of an enum's allowed values: the first of these
/// that the key allows.
pub const RESTRICTIVE: &[&str] = &["deny", "manual", "off", "none", "strict", "ask"];
