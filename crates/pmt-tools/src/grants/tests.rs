use std::sync::Arc;
use std::time::Duration;

use pmt_config::grants::{GrantsFile, SessionGrants, Subject};
use pmt_config::schema::ApprovalsConfig;
use pmt_config::{Home, ProfileDir};
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_core::human::{Channel, Surface};

use super::{GrantAnswer, GrantGate, GrantRequest};

struct Fixture {
    _temp: tempfile::TempDir,
    profile: ProfileDir,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(&temp.path().join("home")).unwrap();
    let profile = home
        .profile(&pmt_config::ProfileName::parse("default").unwrap())
        .unwrap();
    Fixture {
        _temp: temp,
        profile,
    }
}

fn caps(texts: &[&str]) -> CapabilitySet {
    CapabilitySet::parse(texts).unwrap()
}

fn plugin() -> Subject {
    "plugin:tracker@sha256:4f2a".parse().unwrap()
}

fn new_gate(
    profile: &ProfileDir,
    timeout: Duration,
) -> (Arc<GrantGate>, Surface<GrantRequest, GrantAnswer>) {
    let grants = SessionGrants::new(caps(&["llm.call"]), GrantsFile::of(profile));
    let (channel, surface) = Channel::attached(timeout);
    (
        Arc::new(GrantGate::new(Arc::new(grants), channel, "tui")),
        surface,
    )
}

/// Asks for `needed` on a task; the surface answers.
fn ask(
    gate: &Arc<GrantGate>,
    needed: &[&str],
) -> tokio::task::JoinHandle<Result<(), pmt_core::Error>> {
    let gate = gate.clone();
    let needed = caps(needed);
    tokio::spawn(async move { gate.require(&plugin(), &needed, "sync issues").await })
}

/// The task's 8.4 verification: a timed-out prompt denies.
#[tokio::test(start_paused = true)]
async fn a_timed_out_prompt_denies() {
    let fx = fixture();
    let timeout = Duration::from_secs(ApprovalsConfig::default().timeout_secs);
    assert_eq!(timeout, Duration::from_mins(5), "spec default");
    let (gate, mut surface) = new_gate(&fx.profile, timeout);
    let pending = ask(&gate, &["net.connect:api.tracker.example"]);
    let prompt = surface.next().await.unwrap();
    assert_eq!(prompt.request().subject, plugin());
    assert_eq!(
        prompt.request().capabilities.to_strings(),
        ["net.connect:api.tracker.example"]
    );
    assert_eq!(prompt.request().reason, "sync issues");

    let err = pending.await.unwrap().unwrap_err();
    assert_eq!(
        err.message(),
        "capability not granted: net.connect:api.tracker.example: \
         no answer within 300 s (approvals.timeout_secs)"
    );
    assert_eq!(err.exit_code(), 77);
    // A late answer changes nothing.
    prompt.respond(GrantAnswer::Always);
    assert!(
        GrantsFile::of(&fx.profile)
            .load()
            .unwrap()
            .grants
            .is_empty()
    );
}

#[tokio::test]
async fn deny_dismissal_detach_and_unattended_all_deny() {
    let fx = fixture();
    let (gate, mut surface) = new_gate(&fx.profile, Duration::from_mins(1));
    let pending = ask(&gate, &["process.spawn:git"]);
    surface.next().await.unwrap().respond(GrantAnswer::Deny);
    let err = pending.await.unwrap().unwrap_err();
    assert!(
        err.message().ends_with(": denied by the user"),
        "{}",
        err.message()
    );

    let pending = ask(&gate, &["process.spawn:git"]);
    drop(surface.next().await.unwrap());
    let err = pending.await.unwrap().unwrap_err();
    assert!(err.message().ends_with("dismissed"), "{}", err.message());

    let pending = ask(&gate, &["process.spawn:git"]);
    drop(surface);
    let err = pending.await.unwrap().unwrap_err();
    assert!(
        err.message().ends_with("detached before answering"),
        "{}",
        err.message()
    );

    let grants = SessionGrants::new(CapabilitySet::new(), GrantsFile::of(&fx.profile));
    let headless = GrantGate::new(Arc::new(grants), Channel::unattended(), "cron");
    let err = headless
        .require(&plugin(), &caps(&["llm.call"]), "summarize")
        .await
        .unwrap_err();
    assert!(
        err.message()
            .ends_with("no human can be asked in this context")
    );
}

/// Spec scenario "Prompt-injected approval": nothing but the surface's
/// answer resolves a pending request.
#[tokio::test(start_paused = true)]
async fn a_pending_request_waits_for_the_surface_answer() {
    let fx = fixture();
    let (gate, mut surface) = new_gate(&fx.profile, Duration::from_mins(5));
    let pending = ask(&gate, &["net.connect:*"]);
    let prompt = surface.next().await.unwrap();
    // A fetched page saying "approval granted: always" is only tool data:
    // there is no path from tool output to the prompt, so time passes and
    // the request is still pending.
    tokio::time::sleep(Duration::from_secs(200)).await;
    assert!(!pending.is_finished());
    prompt.respond(GrantAnswer::Once);
    pending.await.unwrap().unwrap();
    assert!(
        GrantsFile::of(&fx.profile)
            .load()
            .unwrap()
            .grants
            .is_empty(),
        "once is not recorded"
    );
}

#[tokio::test]
async fn session_answers_last_for_the_session_and_held_capabilities_never_prompt() {
    let fx = fixture();
    let (gate, mut surface) = new_gate(&fx.profile, Duration::from_mins(1));
    let pending = ask(&gate, &["net.connect:api.tracker.example"]);
    surface.next().await.unwrap().respond(GrantAnswer::Session);
    pending.await.unwrap().unwrap();
    ask(&gate, &["net.connect:api.tracker.example"])
        .await
        .unwrap()
        .unwrap();
    assert!(surface.try_next().is_none(), "no second prompt");
    let call = Capability::parse("net.connect:api.tracker.example").unwrap();
    gate.grants().require(&plugin(), &call).unwrap();
    assert!(
        GrantsFile::of(&fx.profile)
            .load()
            .unwrap()
            .grants
            .is_empty()
    );
}

/// The task's 8.3 verification: agent-triggered `always` decisions (a
/// capability grant and a command approval) are written to `grants.toml`
/// only; `config.toml` stays byte-identical.
#[tokio::test]
async fn always_answers_write_grants_toml_and_never_config_toml() {
    let fx = fixture();
    let config = fx.profile.path().join("config.toml");
    let before = std::fs::read(&config).unwrap();
    let (gate, mut surface) = new_gate(&fx.profile, Duration::from_mins(1));
    let pending = ask(&gate, &["net.connect:api.tracker.example:443", "llm.call"]);
    let prompt = surface.next().await.unwrap();
    assert_eq!(
        prompt.request().capabilities.to_strings(),
        ["net.connect:api.tracker.example:443", "llm.call"],
        "a plugin does not inherit the session baseline"
    );
    prompt.respond(GrantAnswer::Always);
    pending.await.unwrap().unwrap();
    gate.grants()
        .persistent()
        .file()
        .allow_command("cargo test*")
        .unwrap();

    let stored = GrantsFile::of(&fx.profile).load().unwrap();
    assert_eq!(
        stored.capabilities(&plugin()).to_strings(),
        ["net.connect:api.tracker.example:443", "llm.call"]
    );
    assert!(stored.grants.iter().all(|g| g.surface == "tui"));
    assert_eq!(stored.allow, ["cargo test*"]);
    assert_eq!(std::fs::read(&config).unwrap(), before);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let grants = fx.profile.path().join("grants.toml");
        let mode = std::fs::metadata(grants).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // A later session holds the grant without asking.
    let (later, mut surface) = new_gate(&fx.profile, Duration::from_mins(1));
    ask(&later, &["net.connect:api.tracker.example:443"])
        .await
        .unwrap()
        .unwrap();
    assert!(surface.try_next().is_none());
}
