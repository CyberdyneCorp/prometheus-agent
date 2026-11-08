//! The Hermes plugin API the python bridge implements (spec: python-bridge,
//! Hermes API compatibility level; Supported `PluginContext` subset;
//! Unsupported APIs fail with a named error; Hermes import shims).
//!
//! This table is the single definition: the static audit reads it, and the
//! bridge hands it to the Python shim, which raises `UnsupportedPluginApi`
//! with these reasons and refuses imports outside [`SHIM_MODULES`].

/// The Hermes release whose documented plugin API the shim implements.
pub const HERMES_COMPAT_LEVEL: &str = "2026.10";

/// Every `PluginContext` member the shim implements.
pub const SUPPORTED: &[&str] = &[
    "register_tool",
    "register_hook",
    "register_middleware",
    "register_command",
    "register_skill",
    "register_system_prompt_section",
    "register_memory_provider",
    "register_context_engine",
    "register_context_reference",
    "register_image_gen_provider",
    "register_video_gen_provider",
    "register_web_search_provider",
    "register_tts_provider",
    "register_transcription_provider",
    "register_secret_source",
    "register_auxiliary_task",
    "register_redaction_patterns",
    "register_locale",
    "register_locale_dir",
    "register_automation_blueprint",
    "register_cli_command",
    "register_provider",
    "register_approval_transport",
    "post_message",
    "apost_message",
    "inject_message",
    "platform_actions",
    "emit",
    "subscribe",
    "dispatch_tool",
    "llm",
    "state",
    "get_config",
    "set_config",
    "profile_name",
    "plugin_id",
    "has_plugin",
    "has_capability",
    "session_cwd",
    "session_surface",
    "on_unload",
    "spawn_task",
    "manifest",
];

/// The platform-adapter reason.
const ADAPTER: &str = "messaging platform adapters run in the host; write a WASM plugin for the `platform-adapter` world";

/// Members the shim refuses by name, with the reason it gives.
pub const UNSUPPORTED: &[(&str, &str)] = &[
    ("register_platform", ADAPTER),
    ("register_platform_handler", ADAPTER),
    ("register_telegram_handler", ADAPTER),
    ("register_slack_action_handler", ADAPTER),
    (
        "register_dashboard_auth_provider",
        "dashboard sign-in is owned by the Prometheus daemon and cannot be extended from Python",
    ),
    (
        "register_login_backend",
        "sign-in backends are owned by the Prometheus host; model-provider plugins get an OAuth flow through the `provider` WASM world",
    ),
    (
        "register_browser_page_backend",
        "browser page backends need in-process access to the browser host; use the browser tools or an MCP server",
    ),
    (
        "inject_gateway_message",
        "use ctx.inject_message(content, role, session_key=..., origin=...), which the bridge supports",
    ),
    (
        "inject_tui_message",
        "use ctx.inject_message(content, role, session_key=...), which the bridge supports",
    ),
];

/// The reason for any other member outside [`SUPPORTED`].
pub const OTHER_REASON: &str = "it is not part of the documented plugin API the bridge implements";

/// The documented Hermes modules the shim provides.
pub const SHIM_MODULES: &[&str] = &[
    "agent.account_usage",
    "agent.memory_provider",
    "agent.context_engine",
    "agent.context_references",
    "agent.provider_base",
    "agent.image_gen_provider",
    "agent.video_gen_provider",
    "agent.web_search_provider",
    "agent.tts_provider",
    "agent.transcription_provider",
    "agent.secret_sources",
    "agent.secret_sources.base",
    "agent.secret_scope",
    "providers",
    "providers.base",
    "hermes_cli.version_info",
    "hermes_cli.auth_oauth_pkce_plugin",
];

/// Top-level Hermes packages whose other modules are refused.
pub const INTERNAL_PACKAGES: &[&str] = &[
    "hermes_cli",
    "hermes_constants",
    "agent",
    "gateway",
    "tools",
    "run_agent",
];

/// Whether `name` is a supported `PluginContext` member.
#[must_use]
pub fn is_supported(name: &str) -> bool {
    SUPPORTED.contains(&name)
}

/// Why `name` is not supported.
#[must_use]
pub fn reason(name: &str) -> &'static str {
    UNSUPPORTED
        .iter()
        .find(|(n, _)| *n == name)
        .map_or(OTHER_REASON, |(_, why)| why)
}

/// The message of `UnsupportedPluginApi` for `name`.
#[must_use]
pub fn unsupported_message(name: &str) -> String {
    format!(
        "ctx.{name}() is not supported by the Prometheus python bridge: {}",
        reason(name)
    )
}

/// Whether importing module `name` is refused (an internal Hermes module
/// the shim does not provide).
#[must_use]
pub fn is_refused_import(name: &str) -> bool {
    let top = name.split('.').next().unwrap_or(name);
    INTERNAL_PACKAGES.contains(&top)
        && !SHIM_MODULES
            .iter()
            .any(|m| *m == name || m.starts_with(&format!("{name}.")))
}

/// The table as JSON, for the Python shim.
#[must_use]
pub fn as_json() -> serde_json::Value {
    let unsupported: serde_json::Map<String, serde_json::Value> = UNSUPPORTED
        .iter()
        .map(|(n, why)| ((*n).to_owned(), (*why).into()))
        .collect();
    serde_json::json!({
        "compat_level": HERMES_COMPAT_LEVEL,
        "supported": SUPPORTED,
        "unsupported": unsupported,
        "other_reason": OTHER_REASON,
        "shim_modules": SHIM_MODULES,
        "internal_packages": INTERNAL_PACKAGES,
    })
}

/// Whether `range` (`requires_hermes`: comma-separated `>=`, `>`, `<=`,
/// `<`, `==`, `!=` clauses over dotted numeric versions; a bare version
/// means `>=`) admits `level`. Like Hermes, an unparseable version is
/// permissive.
#[must_use]
pub fn admits(range: &str, level: &str) -> bool {
    let Some(current) = version_tuple(level) else {
        return true;
    };
    range
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .all(|clause| {
            let (op, target) = split_clause(clause);
            version_tuple(target).is_none_or(|t| compare(op, &current, &t))
        })
}

fn split_clause(clause: &str) -> (&str, &str) {
    for op in [">=", "<=", "==", "!=", ">", "<"] {
        if let Some(rest) = clause.strip_prefix(op) {
            return (op, rest.trim());
        }
    }
    (">=", clause)
}

fn compare(op: &str, current: &[u64; 3], target: &[u64; 3]) -> bool {
    match op {
        ">=" => current >= target,
        "<=" => current <= target,
        "==" => current == target,
        "!=" => current != target,
        ">" => current > target,
        _ => current < target,
    }
}

/// `v2026.10` / `2026.10.1rc1` -> `[2026, 10, 1]`.
fn version_tuple(text: &str) -> Option<[u64; 3]> {
    let text = text.trim().trim_start_matches('v');
    let base = text.split(['-', '+']).next().unwrap_or(text);
    let mut out = [0u64; 3];
    for (slot, part) in out.iter_mut().zip(base.split('.')) {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        *slot = digits.parse().ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_above_the_level_excludes_it() {
        assert!(!admits(">=2027.1", "2026.10"));
        assert!(admits(">=2026.1, <2027", "2026.10"));
        assert!(admits("2026.10", "2026.10"));
        assert!(admits("not a version", "2026.10"));
    }

    #[test]
    fn internal_modules_are_refused_but_documented_ones_are_not() {
        assert!(is_refused_import("gateway.run"));
        assert!(is_refused_import("agent.portal_tags"));
        assert!(!is_refused_import("agent.memory_provider"));
        assert!(!is_refused_import("agent"));
        assert!(!is_refused_import("requests"));
    }

    #[test]
    fn an_adapter_registration_points_to_the_wasm_world() {
        let message = unsupported_message("register_platform");
        assert!(message.starts_with(
            "ctx.register_platform() is not supported by the Prometheus python bridge:"
        ));
        assert!(message.contains("platform-adapter"));
    }
}
