//! Skill preload, stacking and bundles (spec: skills, Skill preload at
//! launch and per-profile auto-load; Stacking skills in one message; Skill
//! bundles).

use std::sync::{Arc, Mutex};

use pmt_agent::turn::PromptSetup;
use pmt_skills::dispatch::{self, STACK_NOTICE};
use pmt_skills::preload;

use crate::support::{
    Profile, Recording, Session, agent, mock_model, skill_md, system_text, turn, write,
};

const ANSWER: &str = "[[rule]]\ntext = \"ok\"\nrepeat = true\n";

fn add(profile: &Profile, name: &str, body: &str) {
    write(
        &profile.skills(),
        &format!("{name}/SKILL.md"),
        &format!("---\nname: {name}\ndescription: The {name} skill.\n---\n\n{body}\n"),
    );
}

struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn preloaded_skills_are_in_the_first_request() {
    let profile = Profile::new("[skills]\nauto_load = [\"test-driven-development\"]\n");
    add(
        &profile,
        "github-pr-workflow",
        "PR-WORKFLOW-BODY: open a draft PR.",
    );
    add(
        &profile,
        "test-driven-development",
        "TDD-BODY: write the failing test first.",
    );
    let session = Session::open(&profile).await;
    let preloaded = preload::preload(
        &session.skills,
        &["github-pr-workflow,test-driven-development".to_owned()],
    )
    .await;
    assert_eq!(
        preloaded.loaded,
        ["github-pr-workflow", "test-driven-development"],
        "a name in both lists loads once"
    );
    let mut prompt = PromptSetup::new("cli", None);
    prompt.pinned_skills.clone_from(&preloaded.text);
    let model = Recording::wrap(mock_model(&profile, ANSWER).await);
    let agent = agent(&profile, &session, model.clone(), prompt);
    turn(&profile, &agent, session.id, "first").await;
    turn(&profile, &agent, session.id, "second").await;
    let requests = model.requests();
    let first = system_text(&requests[0]);
    assert!(
        first.contains("PR-WORKFLOW-BODY") && first.contains("TDD-BODY"),
        "{first}"
    );
    let second = &requests[requests.len() - 1];
    assert_eq!(
        second.system, requests[0].system,
        "prompt prefix byte-identical"
    );
    assert_eq!(
        second.messages[0], requests[0].messages[0],
        "first user message identical"
    );
    let usage = profile
        .store
        .skill_usage("github-pr-workflow")
        .unwrap()
        .unwrap();
    assert_eq!(usage.use_count, 1);
}

#[tokio::test]
async fn missing_auto_load_entry_is_skipped_with_one_warning() {
    let profile = Profile::new("[skills]\nauto_load = [\"gone\", \"off\"]\ndisabled = [\"off\"]\n");
    add(&profile, "off", "Disabled body.");
    let session = Session::open(&profile).await;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || Logs(Arc::clone(&sink)))
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let preloaded = preload::preload(&session.skills, &["missing".to_owned()]).await;
    drop(guard);
    assert_eq!(preloaded.text, None, "the session starts without preloads");
    let skipped: Vec<&str> = preloaded.skipped.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(skipped, ["missing", "gone", "off"]);
    assert_eq!(preloaded.not_found_lines(), ["skill 'missing' not found"]);
    let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let warns: Vec<&str> = text.lines().filter(|l| l.contains("WARN")).collect();
    assert_eq!(
        warns.iter().filter(|l| l.contains("gone")).count(),
        1,
        "{text}"
    );
    assert_eq!(warns.len(), 3, "{text}");
}

#[tokio::test]
async fn stacked_skills_load_into_one_message() {
    let profile = Profile::new("");
    for name in [
        "github-pr-workflow",
        "test-driven-development",
        "pdf",
        "a",
        "b",
        "c",
        "d",
        "e",
    ] {
        add(&profile, name, &format!("{name}-BODY"));
    }
    let session = Session::open(&profile).await;
    let core = ["help", "new"];
    let two = dispatch::expand(
        &session.skills,
        "/github-pr-workflow /test-driven-development fix issue 123",
        &core,
    )
    .await
    .unwrap()
    .unwrap();
    let message = &two.message;
    let first = message.find("github-pr-workflow-BODY").unwrap();
    let second = message.find("test-driven-development-BODY").unwrap();
    assert!(first < second, "{message}");
    assert!(
        message.ends_with("User instruction: fix issue 123"),
        "{message}"
    );

    let path = dispatch::expand(&session.skills, "/pdf /tmp/scan.pdf extract tables", &core)
        .await
        .unwrap()
        .unwrap();
    assert!(path.message.contains("pdf-BODY"));
    assert!(
        path.message
            .ends_with("User instruction: /tmp/scan.pdf extract tables"),
        "{}",
        path.message
    );

    let six = dispatch::expand(&session.skills, "/a /b /c /d /e /pdf go", &core)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(six.notices, [STACK_NOTICE]);
    assert!(!six.message.contains("pdf-BODY"));
    assert!(
        six.message.ends_with("User instruction: /pdf go"),
        "{}",
        six.message
    );

    assert!(
        dispatch::expand(&session.skills, "/help pdf", &core)
            .await
            .is_none()
    );
    assert!(
        dispatch::expand(&session.skills, "just text", &core)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn bundle_loads_its_skills_and_skips_missing_ones() {
    let profile = Profile::new("");
    for name in [
        "github-code-review",
        "test-driven-development",
        "github-pr-workflow",
    ] {
        add(&profile, name, &format!("{name}-BODY"));
    }
    write(
        profile.scope.dir(),
        "skill-bundles/backend-dev.toml",
        "description = \"Backend work.\"\nskills = [\"github-code-review\", \"test-driven-development\", \"github-pr-workflow\", \"skill-foo\"]\ninstruction = \"Run the tests before you finish.\"\n",
    );
    // A skill with the bundle's slug loses to the bundle; a built-in wins
    // over both.
    write(
        &profile.skills(),
        "backend-dev/SKILL.md",
        &skill_md("backend-dev", "Shadowed by the bundle.", ""),
    );
    let session = Session::open(&profile).await;
    let expanded = dispatch::expand(&session.skills, "/backend-dev refactor auth", &["help"])
        .await
        .unwrap()
        .unwrap();
    let message = expanded.message;
    for body in [
        "github-code-review-BODY",
        "test-driven-development-BODY",
        "github-pr-workflow-BODY",
    ] {
        assert!(message.contains(body), "{message}");
    }
    assert!(message.contains("Bundle instruction: Run the tests before you finish."));
    assert!(message.contains("User instruction: refactor auth"));
    assert!(message.contains("skipped: skill-foo"), "{message}");
    assert!(
        !message.contains("# backend-dev"),
        "the bundle wins over the skill"
    );
    assert!(
        dispatch::expand(&session.skills, "/backend-dev x", &["backend-dev"])
            .await
            .is_none()
    );
}
