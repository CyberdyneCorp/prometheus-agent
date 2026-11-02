//! The closed hook registry (spec: plugin-system, Typed hook registry;
//! messaging-gateway, Gateway lifecycle hooks are typed): the only hook
//! names any capability, plugin, harness `hooks.toml` or bridge may use.
//! An unknown name is refused with `unknown hook '<name>'`.

use std::sync::LazyLock;

use crate::error::Error;
use crate::registry::Registry;

/// Every hook name, in the order the spec lists them.
pub const HOOK_NAMES: &[&str] = &[
    "pre_tool_call",
    "post_tool_call",
    "pre_llm_call",
    "post_llm_call",
    "pre_api_request",
    "post_api_request",
    "api_request_error",
    "pre_auxiliary_call",
    "post_auxiliary_call",
    "on_session_start",
    "on_session_end",
    "on_session_finalize",
    "on_session_reset",
    "on_session_delete",
    "on_turn_start",
    "on_turn_end",
    "post_compaction",
    "pre_command",
    "pre_gateway_dispatch",
    "post_gateway_admission",
    "post_gateway_delivery",
    "gateway_platform_event",
    "on_gateway_startup",
    "on_gateway_shutdown",
    "transform_terminal_output",
    "transform_tool_result",
    "transform_llm_output",
    "transform_api_error_classification",
    "on_stream_start",
    "on_stream_delta",
    "on_stream_end",
    "on_interim_message",
    "pre_verify",
    "on_skill_lifecycle",
    "subagent_start",
    "subagent_stop",
    "agent_loop_stopped",
    "pre_approval_request",
    "post_approval_response",
    "on_human_input_request",
    "on_human_input_resolved",
    "on_room_member_activity",
    "pre_transcription",
    "kanban_task_claimed",
    "kanban_task_completed",
    "kanban_task_blocked",
    "on_kanban_worker_spawned",
    "on_kanban_worker_exited",
    "on_kanban_worker_stale_claim",
    "on_kanban_task_updated",
    "on_kanban_dispatch_tick",
    "on_plugin_loaded",
];

static REGISTRY: LazyLock<Registry<()>> = LazyLock::new(|| {
    Registry::from_entries("hook", HOOK_NAMES.iter().map(|n| (*n, ())))
        .expect("hook names are distinct")
});

/// Checks that `name` is a registered hook.
///
/// # Errors
///
/// `registry.unknown_name` with `unknown hook '<name>'` (and the closest
/// names).
pub fn check(name: &str) -> Result<(), Error> {
    REGISTRY.get(name).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_misspelled_hook_is_refused_by_name() {
        let err = check("on_sesion_start").unwrap_err();
        assert!(
            err.message().starts_with("unknown hook 'on_sesion_start'"),
            "{}",
            err.message()
        );
        assert!(check("on_session_start").is_ok());
        assert!(check("pre_command").is_ok());
    }
}
