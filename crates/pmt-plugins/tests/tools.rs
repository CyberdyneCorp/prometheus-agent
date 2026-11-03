//! Plugin tools in the tool registry (spec: plugin-system, Plugin tool
//! registration; WASM sandbox resource limits: the turn continues after a
//! timed-out tool).

mod common;

use std::sync::Arc;

use common::{Fake, Profile, TOOL};
use pmt_core::cancel::CancelToken;
use pmt_plugins::host::Runtime;
use pmt_tools::descriptor::{
    CallContext, HandlerFuture, Owner, RegisteredTool, ToolDescriptor, ToolHandler,
};
use pmt_tools::{Builtins, Status, ToolRegistry, ToolResult};
use serde_json::{Value, json};

/// A built-in `ok` tool answering "builtin".
struct Builtin;

impl ToolHandler for Builtin {
    fn call<'a>(&'a self, _: &'a CallContext, _: Value) -> HandlerFuture<'a> {
        Box::pin(async { Ok(ToolResult::ok("builtin")) })
    }
}

fn registry() -> ToolRegistry {
    let ok = RegisteredTool::new(
        ToolDescriptor::new("ok", "core", "built-in ok", json!({"type": "object"})),
        Owner::Builtin,
        Arc::new(Builtin),
    );
    ToolRegistry::new("default", Arc::new(Builtins::standard(vec![ok]).unwrap()))
}

async fn run(registry: &ToolRegistry, name: &str) -> ToolResult {
    let tool = registry.get(name).unwrap();
    let pmt_tools::descriptor::Executor::Handler(handler) = &tool.executor else {
        panic!("{name} runs through a handler");
    };
    let context = CallContext {
        call_id: "call_1".to_owned(),
        session_id: "ses_test".to_owned(),
        cancel: CancelToken::new(),
    };
    handler.call(&context, json!({})).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_tools_register_under_the_plugin_toolset_and_a_timeout_is_a_result() {
    let mut profile = Profile::new("default");
    profile.config.wasm.call_timeout_ms = 200;
    profile.install("spin", "testing-spin", TOOL, "");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let mut registry = registry();
    let toolsets = loaded.register_tools(&mut registry, "cli");
    assert_eq!(toolsets, vec!["spin"]);
    assert_eq!(
        registry.members("spin").unwrap(),
        vec!["grow", "spin", "trap"],
        "the built-in `ok` stays: no override grant"
    );
    assert_eq!(run(&registry, "ok").await.text_content(), "builtin");
    let spun = run(&registry, "spin").await;
    assert_eq!(spun.status, Status::Timeout);
    assert_eq!(spun.text_content(), "plugin 'spin' exceeded call timeout");
    let trapped = run(&registry, "trap").await;
    assert_eq!(trapped.status, Status::Error);
}

#[tokio::test(flavor = "multi_thread")]
async fn overriding_a_core_tool_needs_the_grant() {
    // Spec scenario "Override needs a grant" (with a core tool `ok`).
    let mut profile = Profile::new("default");
    profile.install("spin", "testing-spin", TOOL, "tools_override = [\"ok\"]");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let mut registry = registry();
    loaded.register_tools(&mut registry, "cli");
    assert_eq!(
        registry.get("ok").unwrap().owner,
        Owner::Plugin("spin".to_owned())
    );
    assert_eq!(run(&registry, "ok").await.text_content(), "fine");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_another_plugin_registered_is_refused_naming_both() {
    let mut profile = Profile::new("default");
    profile.install("alpha", "testing-spin", TOOL, "");
    profile.install("beta", "testing-spin", TOOL, "");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let mut registry = registry();
    loaded.register_tools(&mut registry, "cli");
    assert_eq!(
        registry.get("spin").unwrap().owner,
        Owner::Plugin("alpha".to_owned())
    );
    let refused: Vec<&str> = registry
        .rejections()
        .iter()
        .map(|r| r.message.as_str())
        .collect();
    assert!(
        refused.contains(
            &"tool 'spin' from plugin 'beta' refused: already registered by plugin 'alpha'"
        ),
        "{refused:?}"
    );
}
