//! The layered, frozen system prompt (spec: agent-turn-loop, Layered system
//! prompt; Identity file independent of context files; System prompt
//! frozen per session; Request-time additions never touch history;
//! Personality overlays; Session id in the system prompt).

use std::path::PathBuf;

use pmt_agent::prompt::blocks::{DEFAULT_IDENTITY, RUNTIME_END, RUNTIME_START};
use pmt_agent::turn::{PromptSetup, TurnInput};
use pmt_obs::trajectory::events::Event;
use pmt_providers::request::{Part, Request};
use pmt_store::Source;

use crate::support::{FakeTools, Profile, Run, agent, mock_model, options, system_text};

const ANSWER: &str = "[[rule]]\ntext = \"ok\"\nrepeat = true\n";

fn rebuilds(run: &Run) -> Vec<String> {
    run.events
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::PromptRebuilt(p) => Some(p.reason),
            _ => None,
        })
        .collect()
}

/// Role and parts (cache markers move to the newest message by design).
fn content(
    messages: &[pmt_providers::request::Message],
) -> Vec<(pmt_providers::request::Role, Vec<Part>)> {
    messages.iter().map(|m| (m.role, m.parts.clone())).collect()
}

fn user_text(request: &Request, index: usize) -> String {
    match &request.messages[index].parts[0] {
        Part::Text { text } => text.clone(),
        other => panic!("not text: {other:?}"),
    }
}

async fn session_in(profile: &Profile, cwd: Option<PathBuf>) -> (Run, Request) {
    let s = profile.session(Source::Cli);
    let model = mock_model(profile, ANSWER, "mock").await;
    let run = agent(profile, s, model.clone(), FakeTools::new(), |setup| {
        setup.prompt = PromptSetup::new("cli", cwd);
    });
    run.turn(profile, "hi").await;
    let request = model.requests().remove(0);
    (run, request)
}

#[tokio::test]
async fn the_system_prompt_bytes_are_identical_across_turns() {
    let profile = Profile::new("");
    std::fs::write(profile.dir().join("SOUL.md"), "You are Ada.").unwrap();
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, ANSWER, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    run.turn(&profile, "one").await;
    // Disk changes after the first turn never reach this session.
    std::fs::write(profile.dir().join("SOUL.md"), "You are Grace.").unwrap();
    run.turn(&profile, "two").await;
    run.turn(&profile, "three").await;

    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert_eq!(request.system, requests[0].system);
        assert_eq!(request.tools, requests[0].tools);
    }
    assert_eq!(
        requests[0].system.len(),
        3,
        "static, profile, session tiers"
    );
    assert!(requests[0].system[0].text.starts_with("You are Ada."));
    // Each request extends the previous one's transcript.
    assert_eq!(
        content(&requests[2].messages[..3]),
        content(&requests[1].messages[..3])
    );
    assert_eq!(rebuilds(&run), ["new_session"]);
    let stored = profile.store.session_prompt(s).unwrap().unwrap();
    assert_eq!(
        profile
            .store
            .session(s)
            .unwrap()
            .system_prompt_hash
            .as_deref(),
        Some(stored.sha256.as_str())
    );
}

#[tokio::test]
async fn a_resumed_session_replays_the_stored_bytes() {
    let mut profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let first = mock_model(&profile, ANSWER, "mock").await;
    agent(&profile, s, first.clone(), FakeTools::new(), |_| {})
        .turn(&profile, "day one")
        .await;

    // Next day: identity, personality and memory changed on disk.
    std::fs::write(profile.dir().join("SOUL.md"), "You are someone else.").unwrap();
    profile.reconfigure("[agent]\npersonality = \"pirate\"\n");
    let second = mock_model(&profile, ANSWER, "mock").await;
    let resumed = agent(&profile, s, second.clone(), FakeTools::new(), |setup| {
        setup.prompt.memory = Some("The user likes tea.".into());
    });
    resumed.turn(&profile, "day two").await;
    assert_eq!(second.requests()[0].system, first.requests()[0].system);
    assert!(rebuilds(&resumed).is_empty());

    // A model change rebuilds, recorded with its reason.
    let other = mock_model(&profile, ANSWER, "mock-large").await;
    let switched = agent(&profile, s, other.clone(), FakeTools::new(), |_| {});
    switched.turn(&profile, "day three").await;
    assert_eq!(rebuilds(&switched), ["model_changed"]);
    let text = system_text(&other.requests()[0]);
    assert!(text.contains("You are someone else."), "{text}");
    assert!(text.contains("# Personality: pirate"), "{text}");
}

#[tokio::test]
async fn static_and_profile_tiers_are_shared_across_working_directories() {
    let profile = Profile::new("");
    let (_, a) = session_in(&profile, Some(profile.path("work/a"))).await;
    let (_, b) = session_in(&profile, Some(profile.path("work/b"))).await;
    assert_eq!(a.system[0], b.system[0], "static tier");
    assert_eq!(a.system[1], b.system[1], "profile tier");
    assert_ne!(a.system[2], b.system[2], "session tier differs");
}

#[tokio::test]
async fn the_runtime_block_is_last() {
    let profile = Profile::new("");
    let cwd = profile.path("repo");
    std::fs::write(cwd.join("AGENTS.md"), "Use tabs.").unwrap();
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, ANSWER, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |setup| {
        setup.prompt = PromptSetup::new("cli", Some(cwd.clone()));
        setup.prompt.memory = Some("# Memory\nThe user is Ada.".into());
        setup.prompt.plugin_sections = vec!["# Plugin\nrepo rules".into()];
    });
    run.turn(&profile, "hi").await;
    let text = system_text(&model.requests()[0]);
    let start = text.find(RUNTIME_START).unwrap();
    for earlier in [
        "# Project Context",
        "Use tabs.",
        "The user is Ada.",
        "repo rules",
    ] {
        assert!(
            text.find(earlier).unwrap() < start,
            "{earlier} after runtime"
        );
    }
    assert!(text.ends_with(RUNTIME_END), "{text}");
}

#[tokio::test]
async fn disabling_context_files_keeps_the_identity() {
    let profile = Profile::new("");
    std::fs::write(profile.dir().join("SOUL.md"), "You are Ada.\n").unwrap();
    let cwd = profile.path("repo");
    std::fs::write(cwd.join("AGENTS.md"), "Project rules.").unwrap();
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, ANSWER, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |setup| {
        setup.prompt = PromptSetup::new("cli", Some(cwd.clone()));
        setup.prompt.no_context_files = true;
    });
    run.turn(&profile, "hi").await;
    let text = system_text(&model.requests()[0]);
    assert!(text.starts_with("You are Ada."), "{text}");
    assert!(!text.contains("Project rules."), "{text}");
    assert!(!text.contains("# Project Context"), "{text}");
}

#[tokio::test]
async fn an_empty_or_disabled_identity_falls_back_to_the_default() {
    let mut profile = Profile::new("");
    std::fs::write(profile.dir().join("SOUL.md"), "  \n").unwrap();
    let (_, request) = session_in(&profile, None).await;
    assert!(request.system[0].text.starts_with(DEFAULT_IDENTITY));

    std::fs::write(profile.dir().join("SOUL.md"), "You are Ada.").unwrap();
    profile.reconfigure("[prompt]\nload_identity = false\n");
    let (_, request) = session_in(&profile, None).await;
    assert!(request.system[0].text.starts_with(DEFAULT_IDENTITY));
}

#[tokio::test]
async fn two_profiles_in_one_process_use_their_own_identity() {
    let work = Profile::new("");
    std::fs::write(work.dir().join("SOUL.md"), "You are the work agent.").unwrap();
    let home = work.sibling("home", "");
    std::fs::write(home.dir().join("SOUL.md"), "You are the home agent.").unwrap();
    let (_, w) = session_in(&work, None).await;
    let (_, h) = session_in(&home, None).await;
    assert!(w.system[0].text.starts_with("You are the work agent."));
    assert!(h.system[0].text.starts_with("You are the home agent."));
}

#[tokio::test]
async fn request_time_additions_ride_the_current_user_message_only() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, ANSWER, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let input = TurnInput {
        text: "deploy it".into(),
        additions: vec!["[plugin context] repo is frozen".into()],
    };
    run.turn_with(&profile, input, options()).await;
    run.turn(&profile, "and now?").await;

    let requests = model.requests();
    assert!(user_text(&requests[0], 0).ends_with("[plugin context] repo is frozen"));
    assert!(!system_text(&requests[0]).contains("repo is frozen"));
    // The next request replays that row byte-for-byte and adds nothing to
    // the new message.
    assert_eq!(
        content(&requests[1].messages[..1]),
        content(&requests[0].messages[..1])
    );
    assert_eq!(user_text(&requests[1], 2), "and now?");
    assert_eq!(requests[1].system, requests[0].system);
}

#[tokio::test]
async fn a_personality_switch_waits_for_the_next_session_unless_now() {
    let mut profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, ANSWER, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    run.turn(&profile, "one").await;

    // `/personality pirate` writes the config; this session keeps its prompt.
    profile.reconfigure("[agent]\npersonality = \"pirate\"\n");
    run.turn(&profile, "two").await;
    let requests = model.requests();
    assert_eq!(requests[1].system, requests[0].system);
    assert!(!system_text(&requests[1]).contains("Personality"));

    // The next new session carries the overlay after the identity.
    let (_, fresh) = session_in(&profile, None).await;
    let text = system_text(&fresh);
    assert!(
        text.contains("# Personality: pirate\nSpeak like a pirate"),
        "{text}"
    );
    assert!(text.find("Personality").unwrap() > text.find(DEFAULT_IDENTITY).unwrap());

    // `--now` rebuilds this session's prompt as a recorded cache break.
    run.agent.apply_personality_now("noir");
    run.turn(&profile, "three").await;
    let now = system_text(model.requests().last().unwrap());
    assert!(now.contains("# Personality: noir"), "{now}");
    assert_eq!(rebuilds(&run), ["new_session", "personality --now"]);
}

#[tokio::test]
async fn a_custom_personality_overrides_a_builtin() {
    let profile = Profile::new(
        "[agent]\npersonality = \"pirate\"\n\n[agent.personalities]\npirate = \"Talk like Long John Silver.\"\n",
    );
    let (_, request) = session_in(&profile, None).await;
    let text = system_text(&request);
    assert!(
        text.contains("# Personality: pirate\nTalk like Long John Silver."),
        "{text}"
    );
    assert!(!text.contains("Speak like a pirate"), "{text}");
}

#[tokio::test]
async fn the_session_id_is_added_on_request() {
    let profile = Profile::new("");
    let (_, plain) = session_in(&profile, None).await;
    assert!(!system_text(&plain).contains("Session ID:"));

    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, ANSWER, "mock").await;
    let flagged = agent(&profile, s, model.clone(), FakeTools::new(), |setup| {
        setup.prompt.pass_session_id = true;
    });
    flagged.turn(&profile, "hi").await;
    let text = system_text(&model.requests()[0]);
    assert!(text.contains(&format!("Session ID: {s}")), "{text}");

    let configured = Profile::new("[prompt]\ninclude_session_id = true\n");
    let (run, request) = session_in(&configured, None).await;
    let text = system_text(&request);
    assert!(
        text.contains(&format!("Session ID: {}", run.session)),
        "{text}"
    );
}

#[tokio::test]
async fn platform_hint_overrides_replace_then_append() {
    let profile = Profile::new(
        "[prompt.platform_hints.telegram]\nreplace = \"Short replies.\"\nappend = \"Use Markdown.\"\n",
    );
    let s = profile.session(Source::Gateway);
    let model = mock_model(&profile, ANSWER, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |setup| {
        setup.prompt = PromptSetup::new("telegram", None);
    });
    run.turn(&profile, "hi").await;
    assert_eq!(
        model.requests()[0].system[1].text,
        "Short replies.\nUse Markdown."
    );

    let err = pmt_config::check(
        std::path::Path::new("config.toml"),
        "[prompt.platform_hints.slack]\nprepend = \"x\"\n",
    )
    .unwrap_err();
    assert!(err.message().contains("unknown key"), "{}", err.message());
    assert!(err.message().contains("prepend"), "{}", err.message());
    assert_eq!(err.exit_code(), 78);
}
