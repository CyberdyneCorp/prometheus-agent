//! What plugins contribute beyond tools and hooks (spec: plugin-system,
//! Plugin system-prompt sections are bounded and frozen; Plugin slash
//! commands; Plugin event bus; Provider-object plugins fail closed when
//! unregistered; WIT world versioning and compatibility).

mod common;

use std::time::{Duration, Instant};

use common::{Fake, HOOK, PROVIDER, Profile, instance};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::load::State;
use pmt_plugins::providers::Category;

fn session() -> CallInfo {
    CallInfo {
        session_id: Some("ses_1".to_owned()),
        surface: "cli".to_owned(),
        cwd: None,
    }
}

/// Installs `testing-hooks` as `id` with `capabilities` and `settings`
/// (`key = JSON` pairs).
fn extension(
    profile: &mut Profile,
    fake: &Fake,
    id: &str,
    capabilities: &str,
    settings: &[(&str, &str)],
) {
    profile.install(id, "testing-hooks", HOOK, capabilities);
    for (key, value) in settings {
        fake.set(id, key, value);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_section_id_is_refused_naming_the_first_plugin() {
    // Spec scenario "Duplicate section id refused".
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    for id in ["a", "b"] {
        extension(
            &mut profile,
            &fake,
            id,
            "prompt_section = 100",
            &[("sections", "\"style.guide\""), ("section", "\"be brief\"")],
        );
    }
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    let b = loaded.statuses.iter().find(|s| s.id == "b").unwrap();
    assert_eq!(b.state, State::Error);
    assert_eq!(
        b.problem.as_deref(),
        Some("system prompt section 'style.guide' is already registered by plugin 'a'")
    );
    let sections =
        tokio::task::spawn_blocking(move || loaded.contributions.render_sections(&session()))
            .await
            .unwrap();
    assert_eq!(sections, vec!["style.guide:be brief".to_owned()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn sections_need_the_grant_and_stay_within_their_bounds() {
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    // No grant: nothing registers, the plugin still loads.
    extension(&mut profile, &fake, "nogrant", "", &[("sections", "\"x\"")]);
    // Larger than its 10-character grant: dropped when rendered.
    extension(
        &mut profile,
        &fake,
        "over",
        "prompt_section = 10",
        &[("sections", "\"over\""), ("section_len", "\"11\"")],
    );
    // Two 4000-character sections fill the 8000 total; the third is
    // dropped in load order.
    for id in ["p1", "p2", "p3"] {
        extension(
            &mut profile,
            &fake,
            id,
            "prompt_section = 4000",
            &[
                ("sections", &format!("\"{id}\"")),
                ("section_len", "\"4000\""),
            ],
        );
    }
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    let nogrant = loaded.statuses.iter().find(|s| s.id == "nogrant").unwrap();
    assert_eq!(nogrant.state, State::Loaded);
    assert!(
        nogrant
            .problem
            .as_deref()
            .unwrap()
            .contains("capability_denied: prompt_section"),
        "{nogrant:?}"
    );
    let ids: Vec<&str> = loaded
        .contributions
        .sections
        .iter()
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(ids, ["over", "p1", "p2", "p3"]);
    let rendered =
        tokio::task::spawn_blocking(move || loaded.contributions.render_sections(&session()))
            .await
            .unwrap();
    assert_eq!(rendered.len(), 2);
    assert!(rendered.iter().all(|s| s.len() == 4000));
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_register_only_when_granted_and_run_in_the_plugin() {
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    extension(
        &mut profile,
        &fake,
        "weather",
        "commands = [\"forecast\"]",
        &[("commands", "\"forecast,radar\"")],
    );
    extension(
        &mut profile,
        &fake,
        "zulu",
        "commands = [\"forecast\"]",
        &[("commands", "\"forecast\"")],
    );
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    let names: Vec<&str> = loaded
        .contributions
        .commands
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["forecast"]);
    let weather = loaded.statuses.iter().find(|s| s.id == "weather").unwrap();
    assert!(
        weather
            .problem
            .as_deref()
            .unwrap_or_default()
            .contains("capability_denied: commands radar"),
        "{weather:?}"
    );
    // Plugins load in id order: `weather` registered it first.
    let zulu = loaded.statuses.iter().find(|s| s.id == "zulu").unwrap();
    assert_eq!(
        zulu.problem.as_deref(),
        Some("slash command '/forecast' is already registered by plugin 'weather'")
    );
    let command = loaded.contributions.command("forecast").unwrap().clone();
    let answer = tokio::task::spawn_blocking(move || command.run(&session(), "berlin"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.unwrap(), "ran /forecast berlin in ses_1");
}

/// The `events` state value of `id` once it holds `want` events.
async fn events_of(loaded: &pmt_plugins::load::Loaded, id: &str, want: usize) -> String {
    let instance = instance(loaded, id);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let seen = instance
            .context()
            .state
            .get("events")
            .unwrap()
            .map(|b| String::from_utf8(b).unwrap())
            .unwrap_or_default();
        if seen.matches(';').count() >= want || Instant::now() > deadline {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn events_reach_subscribers_in_order_and_a_failing_one_does_not_affect_the_emitter() {
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    extension(
        &mut profile,
        &fake,
        "emitter",
        "commands = [\"emit\"]",
        &[("commands", "\"emit\"")],
    );
    extension(
        &mut profile,
        &fake,
        "listener",
        "",
        &[("subscribe", "\"emitter:*\"")],
    );
    extension(
        &mut profile,
        &fake,
        "broken",
        "",
        &[("subscribe", "\"*\""), ("event_mode", "\"error\"")],
    );
    extension(
        &mut profile,
        &fake,
        "elsewhere",
        "",
        &[("subscribe", "\"other:*\"")],
    );
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    let emit = loaded.contributions.command("emit").unwrap().clone();
    for name in ["one", "two", "three"] {
        let emit = emit.clone();
        let answer = tokio::task::spawn_blocking(move || emit.run(&session(), name))
            .await
            .unwrap()
            .unwrap();
        // The emitter's call succeeds whatever the subscribers do.
        assert!(answer.is_ok(), "{answer:?}");
    }
    assert_eq!(
        events_of(&loaded, "listener", 3).await,
        "emitter:one={};emitter:two={};emitter:three={};"
    );
    assert_eq!(events_of(&loaded, "elsewhere", 0).await, "");
    // The embedder's port observes every emit too.
    assert_eq!(fake.seen.events.lock().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_provider_does_not_fall_back_and_a_registered_one_answers() {
    // Spec scenario "Missing image provider does not fall back": nothing
    // registers `fal`, so the category answers the not-installed error
    // even though another image provider (`pics`) is registered.
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install("media", "testing-provider", PROVIDER, "");
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    let err = loaded
        .providers
        .require(Category::ImageGen, "fal")
        .unwrap_err();
    assert_eq!(
        err.message(),
        "image_gen provider 'fal' is not installed or not enabled"
    );
    for category in [
        Category::VideoGen,
        Category::WebSearch,
        Category::WebExtract,
        Category::Tts,
        Category::Stt,
        Category::SecretSource,
    ] {
        let err = loaded.providers.require(category, "fal").unwrap_err();
        assert_eq!(
            err.message(),
            format!(
                "{} provider 'fal' is not installed or not enabled",
                category.as_str()
            )
        );
    }
    let pics = loaded
        .providers
        .require(Category::ImageGen, "pics")
        .unwrap()
        .clone();
    let answer =
        tokio::task::spawn_blocking(move || pics.invoke(&session(), r#"{"prompt":"a cat"}"#))
            .await
            .unwrap();
    assert_eq!(answer.unwrap(), r#"{"prompt":"a cat"}"#);
    let web = loaded
        .providers
        .web_backend(Category::WebSearch, "plugsearch", &session())
        .unwrap();
    let hits = web.search("rust", 5).await.unwrap();
    assert_eq!(hits[0].title, "hit for rust");
    assert!(web.can_extract());
    let page = web.extract("https://example.org/x").await.unwrap();
    assert_eq!(page.markdown, "# https://example.org/x");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_built_against_an_older_hook_minor_still_loads() {
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install(
        "legacy",
        "testing-legacy-hook",
        "\"hook@1.0\"",
        "hooks = [\"pre_llm_call\"]",
    );
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    let legacy = instance(&loaded, "legacy");
    assert_eq!(legacy.exported()[0].to_string(), "hook@1.0.0");
    assert!(!legacy.extends());
    let context = loaded.hooks.pre_llm_call("ses_1", "hi", &session()).await;
    assert_eq!(context.as_deref(), Some("legacy"));
}
