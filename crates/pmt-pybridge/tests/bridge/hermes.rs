//! Real Hermes plugins and the API rules (spec: python-bridge, Supported
//! `PluginContext` subset; Unsupported APIs fail with a named error; Hermes
//! import shims; Hook name and payload translation).

use std::time::Duration;

use serde_json::json;

use crate::rig::{quick, rig};

#[test]
fn the_hermes_security_guidance_plugin_warns_on_a_dangerous_write() {
    let rig = rig("security-guidance", quick());
    let registrations = rig
        .supervisor
        .registrations()
        .unwrap_or_else(|| rig.supervisor.start_now().unwrap());
    assert_eq!(
        registrations.hooks,
        ["pre_tool_call", "transform_tool_result"]
    );
    let payload = json!({
        "tool": "write_file", "arguments": {"path": "a.py", "content": "import pickle\npickle.load(f)\n"},
        "result": "{\"ok\": true}", "status": "ok", "session_id": "s", "call_id": "c",
    });
    let answer = rig
        .supervisor
        .call(
            "hook.invoke",
            &json!({"hook": "transform_tool_result", "payload": payload}),
            Duration::from_secs(20),
        )
        .unwrap();
    let rewritten = answer["results"][0].as_str().unwrap();
    assert!(rewritten.starts_with("{\"ok\": true}"), "{rewritten}");
    assert!(rewritten.contains("Security guidance"), "{rewritten}");
}

#[test]
fn a_platform_adapter_registration_fails_with_a_named_error() {
    let rig = rig("platform", quick());
    let err = rig.supervisor.start_now().unwrap_err();
    assert!(
        err.message().starts_with(
            "ctx.register_platform() is not supported by the Prometheus python bridge:"
        ),
        "{}",
        err.message()
    );
    assert!(
        err.message().contains("platform-adapter"),
        "{}",
        err.message()
    );
}

#[test]
fn an_unsupported_call_from_a_handler_raises_by_name() {
    let rig = rig("echo", quick());
    let err = rig
        .supervisor
        .tool("unsupported", &json!({}), &json!({}))
        .unwrap_err();
    assert!(
        err.message().starts_with(
            "ctx.register_telegram_handler() is not supported by the Prometheus python bridge:"
        ),
        "{}",
        err.message()
    );
}

#[test]
fn an_internal_import_is_refused_by_name() {
    let rig = rig("internal", quick());
    let err = rig.supervisor.start_now().unwrap_err();
    assert!(err.message().contains("ImportError"), "{}", err.message());
    assert!(err.message().contains("'gateway.run'"), "{}", err.message());
    assert!(
        err.message().contains("documented Hermes plugin API"),
        "{}",
        err.message()
    );
}
