//! The WASM host: resource limits, host interfaces under grants and
//! profile isolation (spec: plugin-system, WASM sandbox resource limits;
//! Host interfaces offered to plugins; Capability declarations; Plugin
//! state and profile scope; WIT world versioning and compatibility).

mod common;

use std::time::{Duration, Instant};

use common::{Fake, Profile, TOOL, call, instance};
use pmt_core::capability::Capability;
use pmt_plugins::grants::revoke;
use pmt_plugins::host::Runtime;
use pmt_plugins::load::State;

#[tokio::test(flavor = "multi_thread")]
async fn an_infinite_loop_is_interrupted_within_the_bound() {
    // Spec scenario "Infinite loop is interrupted" (with a 300 ms bound).
    let mut profile = Profile::new("default");
    profile.config.wasm.call_timeout_ms = 300;
    profile.install("spin", "testing-spin", TOOL, "");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let spin = instance(&loaded, "spin");

    let started = Instant::now();
    let err = call(&spin, "spin", "").await.unwrap_err();
    let took = started.elapsed();
    assert_eq!(err.message(), "plugin 'spin' exceeded call timeout");
    assert!(
        took >= Duration::from_millis(250),
        "stopped early: {took:?}"
    );
    assert!(
        took < Duration::from_secs(3),
        "not interrupted in time: {took:?}"
    );
    // The plugin keeps serving (a fresh instance after the trap).
    assert_eq!(call(&spin, "ok", "").await.unwrap().unwrap(), "fine");
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_is_capped_and_three_traps_suspend_the_plugin() {
    let mut profile = Profile::new("default");
    profile.config.wasm.max_memory_bytes = 16 * 1024 * 1024;
    profile.install("spin", "testing-spin", TOOL, "");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let spin = instance(&loaded, "spin");

    let err = call(&spin, "grow", "").await.unwrap_err();
    assert!(
        err.message().starts_with("plugin 'spin' trapped:"),
        "{}",
        err.message()
    );
    assert_eq!(call(&spin, "ok", "").await.unwrap().unwrap(), "fine");
    let err = call(&spin, "trap", "").await.unwrap_err();
    assert!(err.message().starts_with("plugin 'spin' trapped:"));
    assert!(!spin.suspended());
    call(&spin, "trap", "").await.unwrap_err();
    assert!(spin.suspended());
    let err = call(&spin, "ok", "").await.unwrap_err();
    assert_eq!(
        err.message(),
        "plugin 'spin' is suspended after 3 traps within 60s"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_undeclared_host_is_blocked_before_any_connection() {
    // Spec scenario "Undeclared host is blocked".
    let mut profile = Profile::new("default");
    profile.install(
        "probe",
        "testing-probe",
        TOOL,
        "network = [\"api.weather.example\"]",
    );
    let fake = Fake::new();
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    let probe = instance(&loaded, "probe");

    let denied = call(&probe, "fetch", "https://exfil.example.net/upload")
        .await
        .unwrap();
    assert_eq!(
        denied.unwrap_err(),
        "capability_denied: network exfil.example.net"
    );
    assert!(
        fake.seen.http.lock().unwrap().is_empty(),
        "no request reached the network"
    );

    let allowed = call(&probe, "fetch", "https://api.weather.example/today")
        .await
        .unwrap();
    assert_eq!(allowed.unwrap(), "200 sunny");
    assert_eq!(
        *fake.seen.http.lock().unwrap(),
        vec!["https://api.weather.example/today"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_outside_the_grant_is_not_readable_and_revocation_is_immediate() {
    // Spec scenario "Secret outside the grant is not readable".
    let mut profile = Profile::new("default");
    let identity = profile.install(
        "probe",
        "testing-probe",
        TOOL,
        "secrets = [\"NOTION_API_KEY\"]",
    );
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let probe = instance(&loaded, "probe");

    let denied = call(&probe, "secret", "OPENAI_API_KEY").await.unwrap();
    assert_eq!(
        denied.unwrap_err(),
        "capability_denied: secrets OPENAI_API_KEY"
    );
    assert_eq!(
        call(&probe, "secret", "NOTION_API_KEY")
            .await
            .unwrap()
            .unwrap(),
        "secret_n"
    );

    // A revoked capability takes effect on the next call.
    let secret = Capability::parse("secrets.read:NOTION_API_KEY").unwrap();
    revoke(&profile.grants(), &identity.id, Some(&secret)).unwrap();
    let revoked = call(&probe, "secret", "NOTION_API_KEY").await.unwrap();
    assert_eq!(
        revoked.unwrap_err(),
        "capability_denied: secrets NOTION_API_KEY"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_profiles_get_two_instances() {
    // Spec scenario "Two profiles get two instances".
    let mut a = Profile::new("a");
    let mut b = Profile::new("b");
    a.install("notes", "testing-probe", TOOL, "");
    b.install("notes", "testing-probe", TOOL, "");
    let runtime = Runtime::new().unwrap();
    let fake = Fake::new();
    let loaded_a = a.load(&runtime, &fake.ports());
    let loaded_b = b.load(&runtime, &fake.ports());
    let in_a = instance(&loaded_a, "notes");
    let in_b = instance(&loaded_b, "notes");

    assert_eq!(
        call(&in_a, "put", "from-a").await.unwrap().unwrap(),
        "stored"
    );
    assert_eq!(call(&in_a, "get", "").await.unwrap().unwrap(), "from-a");
    assert_eq!(call(&in_b, "get", "").await.unwrap().unwrap(), "<none>");
    assert!(a.path().join("plugin-data/notes/kv").is_dir());
    assert!(!b.path().join("plugin-data/notes/kv").exists());
    assert_eq!(
        call(&in_b, "session", "").await.unwrap().unwrap(),
        "b|cli|ses_test|"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn model_calls_need_the_llm_grant_and_overrides_need_their_own() {
    // Spec scenario "Model override without grant".
    let mut profile = Profile::new("default");
    profile.install("probe", "testing-probe", TOOL, "llm = { enabled = true }");
    let fake = Fake::new();
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    let probe = instance(&loaded, "probe");

    assert_eq!(
        call(&probe, "llm", "").await.unwrap().unwrap(),
        "model says hi"
    );
    let denied = call(&probe, "llm_model", "gpt-5").await.unwrap();
    assert_eq!(denied.unwrap_err(), "capability_denied: llm model override");
    let calls = fake.seen.llm.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].plugin, "probe");
    assert_eq!(calls[0].session_id.as_deref(), Some("ses_test"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ungranted_imports_are_denied_and_always_on_ones_work() {
    let mut profile = Profile::new("default");
    profile.install("probe", "testing-probe", TOOL, "");
    let fake = Fake::new();
    fake.set("probe", "colour", "\"blue\"");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    let probe = instance(&loaded, "probe");

    let llm = call(&probe, "llm", "").await.unwrap();
    assert_eq!(llm.unwrap_err(), "capability_denied: llm");
    let tool = call(&probe, "dispatch", "web_search").await.unwrap();
    assert_eq!(
        tool.unwrap_err(),
        "capability_denied: tools_call web_search"
    );
    assert_eq!(
        call(&probe, "emit", "updated").await.unwrap().unwrap(),
        "emitted"
    );
    assert_eq!(*fake.seen.events.lock().unwrap(), vec!["probe:updated"]);
    let reserved = call(&probe, "emit", "prometheus:session_start")
        .await
        .unwrap();
    assert!(reserved.unwrap_err().starts_with("invalid event name"));
    assert_eq!(
        call(&probe, "setting", "colour").await.unwrap().unwrap(),
        "\"blue\""
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_plugin_does_not_load_until_approved_again() {
    let mut profile = Profile::new("default");
    profile.install(
        "probe",
        "testing-probe",
        TOOL,
        "secrets = [\"NOTION_API_KEY\"]",
    );
    // Editing any file of the plugin changes its identity.
    std::fs::write(profile.plugin_dir("probe").join("README.md"), "changed").unwrap();
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    assert!(loaded.plugins.is_empty());
    let status = &loaded.statuses[0];
    assert_eq!(status.state, State::Error);
    assert!(
        status
            .problem
            .as_deref()
            .unwrap()
            .contains("changed since its capabilities were approved"),
        "{status:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declared_world_must_be_exported() {
    let mut profile = Profile::new("default");
    profile.install("probe", "testing-probe", "\"tool@1.0\", \"hook@1.0\"", "");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    assert_eq!(
        loaded.statuses[0].problem.as_deref(),
        Some(
            "plugin 'probe' declares world 'hook@1.0.0' but its component does not export prometheus:hook/plugin"
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_sample_plugin_compiles_and_imports_only_the_host_interfaces() {
    let runtime = Runtime::new().unwrap();
    for world in [
        "tool",
        "hook",
        "provider",
        "platform-adapter",
        "memory-provider",
        "context-engine",
        "terminal-backend",
        "ui-panel",
        "delegation-router",
    ] {
        let bytes = pmt_plugins::testkit::component(&format!("sample-{world}"));
        let component = runtime.compile(world, &bytes).unwrap();
        let exported = runtime.exported_worlds(&component);
        assert_eq!(
            exported.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![format!("{world}@1.0.0")]
        );
        for import in runtime.imports(&component) {
            assert!(
                import.starts_with("prometheus:host/"),
                "{world} imports {import}"
            );
        }
    }
}

#[test]
fn host_calls_work_outside_an_async_runtime() {
    let mut profile = Profile::new("default");
    profile.install(
        "probe",
        "testing-probe",
        TOOL,
        "network = [\"api.weather.example\"]",
    );
    let fake = Fake::new();
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    let probe = instance(&loaded, "probe");
    let call = pmt_plugins::host::CallInfo::default();
    let fetched = probe
        .call_tool(&call, "fetch", r#"{"v":"https://api.weather.example/x"}"#)
        .unwrap();
    assert_eq!(fetched.unwrap(), "200 sunny");
}
