//! What plugins register through hook world 1.2 and provider world 1.1,
//! and the messaging imports (spec: plugin-system, Model-provider
//! plugins; Plugin CLI subcommands; Approval transports; Plugin live
//! messages and message injection; Platform actions; Transform hooks
//! rewrite results and replies).

mod common;

use common::{Fake, HOOK, PROVIDER, Profile, instance};
use pmt_config::schema::Config;
use pmt_plugins::host::surfaces_world::Terminal;
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::load::{Loaded, State};
use pmt_providers::registry::Origin;
use pmt_providers::{PluginProvider, ProviderRegistry};

fn session() -> CallInfo {
    CallInfo {
        session_id: Some("ses_1".to_owned()),
        surface: "gateway".to_owned(),
        cwd: None,
    }
}

fn problem(loaded: &Loaded, id: &str) -> String {
    let status = loaded.statuses.iter().find(|s| s.id == id).unwrap();
    assert_eq!(status.state, State::Loaded, "{status:?}");
    status.problem.clone().unwrap_or_default()
}

fn registry(loaded: &Loaded) -> ProviderRegistry {
    let plugins = loaded
        .surfaces
        .models
        .iter()
        .map(|m| PluginProvider {
            descriptor: m.descriptor.clone(),
            replaces_builtin: m.replaces_builtin,
        })
        .collect();
    ProviderRegistry::for_profile(&Config::default(), plugins).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_cannot_silently_replace_a_builtin_provider() {
    // Spec scenario "Plugin cannot silently replace a built-in": the
    // registration is refused naming the built-in, which stays active.
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install(
        "llms",
        "testing-provider",
        PROVIDER,
        "providers = [\"openrouter\", \"acme\"]",
    );
    fake.set("llms", "model_ids", "\"openrouter,acme\"");
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    let problem = problem(&loaded, "llms");
    assert!(
        problem.contains("model provider 'openrouter' is a built-in provider")
            && problem.contains("providers_override"),
        "{problem}"
    );
    let ids: Vec<&str> = loaded
        .surfaces
        .models
        .iter()
        .map(|m| m.descriptor.id.as_str())
        .collect();
    assert_eq!(ids, ["acme"]);
    let registry = registry(&loaded);
    assert_eq!(
        registry.lookup("openrouter").unwrap().origin(),
        Origin::Builtin
    );
    assert_eq!(registry.lookup("acme").unwrap().origin(), Origin::Plugin);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_granted_override_replaces_the_builtin_and_an_ungranted_id_is_refused() {
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install(
        "llms",
        "testing-provider",
        PROVIDER,
        "providers = [\"openrouter\"]\nproviders_override = [\"openrouter\"]",
    );
    fake.set("llms", "model_ids", "\"openrouter,stranger\"");
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    let problem = problem(&loaded, "llms");
    assert!(
        problem.contains("capability_denied: providers stranger"),
        "{problem}"
    );
    let registry = registry(&loaded);
    let openrouter = registry.lookup("openrouter").unwrap();
    assert_eq!(openrouter.origin(), Origin::Plugin);
    assert_eq!(openrouter.base_url(), Some("https://llm.example.com/v1"));
    assert!(registry.lookup("stranger").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ungranted_platform_verb_returns_a_structured_error_without_an_adapter_call() {
    // Spec scenario "Ungranted verb returns structured error".
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install(
        "acts",
        "testing-hooks",
        HOOK,
        "commands = [\"react\", \"retitle\"]\nplatform_actions = [\"add_reaction\"]",
    );
    fake.set("acts", "commands", "\"react,retitle\"");
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    let command = loaded.contributions.command("retitle").unwrap().clone();
    let answer = tokio::task::spawn_blocking(move || command.run(&session(), "discord c1 t1"))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        answer,
        "ok=false action=set_thread_title error=capability_not_granted detail=platform_actions does not grant 'set_thread_title'"
    );
    assert!(fake.seen.messaging.lock().unwrap().is_empty());
    let command = loaded.contributions.command("react").unwrap().clone();
    let answer = tokio::task::spawn_blocking(move || command.run(&session(), "discord c1 m1"))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(answer, "ok=true action=add_reaction error= detail=");
    assert_eq!(
        *fake.seen.messaging.lock().unwrap(),
        ["action add_reaction discord"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn injection_needs_its_grant_and_live_messages_bind_to_the_calling_session() {
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install(
        "talk",
        "testing-hooks",
        HOOK,
        "commands = [\"inject\", \"post\"]",
    );
    fake.set("talk", "commands", "\"inject,post\"");
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    let inject = loaded.contributions.command("inject").unwrap().clone();
    let answer = tokio::task::spawn_blocking(move || inject.run(&session(), "ses_2"))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(answer, "injected=false");
    let post = loaded.contributions.command("post").unwrap().clone();
    let answer = tokio::task::spawn_blocking(move || post.run(&session(), "Migrating"))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(answer, "edited fallback=false");
    assert_eq!(
        *fake.seen.messaging.lock().unwrap(),
        [
            "post ses_1 Migrating",
            "edit ses_1 m1 Migrating 1",
            "edit ses_1 m1 Migrating 2",
            "edit ses_1 m1 Migrating 3",
        ]
    );
    // Without a session there is no conversation to post to.
    let post = loaded.contributions.command("post").unwrap().clone();
    let answer = tokio::task::spawn_blocking(move || post.run(&CallInfo::default(), "x"))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(answer, "post failed no-conversation");
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_commands_transports_and_held_streams_need_their_grants() {
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install(
        "tools",
        "testing-hooks",
        HOOK,
        "cli_commands = [\"status\"]\nhooks = [\"transform_llm_output\"]",
    );
    for (key, value) in [
        ("cli", "\"status,secret\""),
        ("transports", "\"pager\""),
        ("hooks", "\"transform_llm_output\""),
        ("hold", "\"transform_llm_output,post_tool_call\""),
    ] {
        fake.set("tools", key, value);
    }
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    let problem = problem(&loaded, "tools");
    for expected in [
        "capability_denied: cli_commands secret",
        "capability_denied: approval_transport",
        "hold_stream on 'post_tool_call' is ignored",
    ] {
        assert!(problem.contains(expected), "{expected}: {problem}");
    }
    let names: Vec<&str> = loaded
        .surfaces
        .cli
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["status"]);
    assert!(loaded.surfaces.transports.is_empty());
    assert!(loaded.hooks.holds_stream());
    let tools = instance(&loaded, "tools");
    let output = tokio::task::spawn_blocking(move || {
        tools.run_cli(
            &CallInfo::default(),
            "status",
            &["now".to_owned()],
            Terminal {
                columns: 80,
                rows: 24,
                tty: false,
            },
        )
    })
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(output.code, 0);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "ran status now as default tty=false cols=80\n"
    );
}
