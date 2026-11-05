//! Real Hermes model-provider plugins load unchanged and their request
//! quirks reach the host (spec: python-bridge, Model-provider plugins
//! through the bridge). Fixtures are copied from hermes-agent.

use std::time::Duration;

use serde_json::{Value, json};

use crate::rig::{quick, rig};

fn provider_call(rig: &crate::rig::Rig, provider: &str, method: &str, args: &Value) -> Value {
    rig.supervisor
        .call(
            "provider.call",
            &json!({"kind": "model", "provider": provider, "method": method, "args": args}),
            Duration::from_secs(20),
        )
        .unwrap()
}

#[test]
fn the_hermes_nvidia_plugin_strips_tool_names_and_caps_output() {
    let rig = rig("nvidia", quick());
    let registrations = rig.supervisor.start_now().unwrap();
    let profile = &registrations.providers[0];
    assert_eq!(profile["name"], "nvidia");
    assert_eq!(profile["base_url"], "https://integrate.api.nvidia.com/v1");
    assert!(
        profile["hooks"]
            .as_array()
            .unwrap()
            .contains(&json!("prepare_messages"))
    );
    let body = json!({
        "model": "nvidia/llama-3.3-70b-instruct",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "tool", "tool_call_id": "c1", "name": "clock", "content": "noon"},
        ],
    });
    let prepared = provider_call(
        &rig,
        "nvidia",
        "prepare_request",
        &json!({"body": body, "context": {"model": "nvidia/llama-3.3-70b-instruct"}}),
    );
    assert_eq!(prepared["max_tokens"], 16384, "{prepared}");
    assert_eq!(
        prepared["messages"][1],
        json!({"role": "tool", "tool_call_id": "c1", "content": "noon"})
    );
    let explicit = provider_call(
        &rig,
        "nvidia",
        "prepare_request",
        &json!({"body": {"model": "m", "max_tokens": 10, "messages": []}, "context": {}}),
    );
    assert_eq!(explicit["max_tokens"], 10, "the caller's cap wins");
}

#[test]
fn the_hermes_gmi_plugin_resolves_its_lazy_user_agent() {
    let rig = rig("gmi", quick());
    let registrations = rig.supervisor.start_now().unwrap();
    assert_eq!(registrations.providers[0]["name"], "gmi");
    let headers = provider_call(&rig, "gmi", "headers", &json!({"context": {}}));
    assert_eq!(headers["User-Agent"], "HermesAgent/2026.10", "{headers}");
}

#[test]
fn the_hermes_xiaomi_plugin_loads_with_its_capability_fields() {
    let rig = rig("xiaomi", quick());
    let registrations = rig.supervisor.start_now().unwrap();
    assert_eq!(registrations.providers[0]["name"], "xiaomi");
    assert!(registrations.degraded.is_empty(), "{registrations:?}");
}
