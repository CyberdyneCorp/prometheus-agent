//! `/learn`, `/refine`, the write-approval review surface and
//! `/reload-skills`, driven through the mock provider (spec: skills,
//! /learn distils a skill from sources; /refine runs self-improvement
//! review on demand; Skill write-approval review surface; /reload-skills
//! rescans roots).

use std::sync::Arc;

use pmt_agent::turn::PromptSetup;
use pmt_agent::turn::tools::{SessionTools, Timings};
use pmt_skills::layout::SKILL_FILE;
use pmt_skills::lint::{self, LintContext};
use pmt_skills::review::{self, ReviewRun};
use pmt_skills::{activation, dispatch, learn, pending, refine, reload};
use pmt_store::{NewMessage, Role};
use serde_json::json;

use crate::support::{
    Profile, Recording, Session, agent, dispatcher, mock_model, system_text, turn, write,
};

const FOO: &str = "---\nname: foo\ndescription: Foo things.\n---\n\nUse the old way.\n";

const PATCH_SCRIPT: &str = r#"
[[rule]]
user = "improve foo"
[[rule.tool_calls]]
name = "skill_manage"
arguments = { action = "patch", name = "foo", old_string = "old way", new_string = "new way" }

[[rule]]
tool_result_regex = "pending write"
text = "staged"
"#;

#[tokio::test]
async fn staged_write_applies_only_on_approval() {
    let profile = Profile::new("[skills]\nwrite_approval = true\n");
    write(&profile.skills(), "foo/SKILL.md", FOO);
    let session = Session::open(&profile).await;
    let model = mock_model(&profile, PATCH_SCRIPT).await;
    let agent = agent(&profile, &session, model, PromptSetup::new("cli", None));
    let result = turn(&profile, &agent, session.id, "please improve foo").await;
    assert_eq!(result.final_response.as_deref(), Some("staged"));

    let staged = pending::list(profile.scope.dir());
    assert_eq!(staged.len(), 1);
    let id = staged[0].id.clone();
    assert!(
        profile
            .scope
            .dir()
            .join(format!("pending/skills/{id}.json"))
            .is_file(),
        "the record survives restarts on disk"
    );
    let before = session.json("skill_view", json!({"name": "foo"})).await;
    assert!(before["content"].as_str().unwrap().contains("old way"));

    // A new session (a restart) sees the same record.
    let later = Session::open(&profile).await;
    let listing = activation::run(&later.skills, "pending", &[]).unwrap().text;
    assert!(
        listing.contains(&id) && listing.contains("patch foo SKILL.md"),
        "{listing}"
    );
    let diff = activation::run(&later.skills, &format!("diff {id}"), &[])
        .unwrap()
        .text;
    assert!(
        diff.contains("-Use the old way.") && diff.contains("+Use the new way."),
        "{diff}"
    );

    activation::run(&later.skills, &format!("approve {id}"), &[]).unwrap();
    let after = later.json("skill_view", json!({"name": "foo"})).await;
    assert!(after["content"].as_str().unwrap().contains("new way"));
    assert!(
        pending::list(profile.scope.dir()).is_empty(),
        "record removed"
    );
}

#[tokio::test]
async fn approve_and_reject_all_and_the_approval_toggle() {
    let profile = Profile::new("");
    let session = Session::open(&profile).await;
    let reply = activation::run(&session.skills, "approval on", &[]).unwrap();
    assert!(reply.text.contains("staged"), "{}", reply.text);
    let config = profile
        .scope
        .profile()
        .config_file()
        .raw()
        .unwrap()
        .unwrap();
    assert!(config.contains("write_approval = true"), "{config}");
    for name in ["one", "two"] {
        let content = format!("---\nname: {name}\ndescription: The {name} skill.\n---\n\nBody.\n");
        session
            .json(
                "skill_manage",
                json!({"action": "create", "name": name, "content": content}),
            )
            .await;
    }
    assert_eq!(pending::list(profile.scope.dir()).len(), 2);
    activation::run(&session.skills, "approve all", &[]).unwrap();
    assert!(profile.skills().join("one/SKILL.md").is_file());
    assert!(profile.skills().join("two/SKILL.md").is_file());
    session
        .json(
            "skill_manage",
            json!({"action": "create", "name": "three", "content": "---\nname: three\ndescription: Three.\n---\n\nBody.\n"}),
        )
        .await;
    activation::run(&session.skills, "reject all", &[]).unwrap();
    assert!(pending::list(profile.scope.dir()).is_empty());
    assert!(!profile.skills().join("three").exists());
    activation::run(&session.skills, "approval off", &[]).unwrap();
    session
        .json(
            "skill_manage",
            json!({"action": "create", "name": "four", "content": "---\nname: four\ndescription: Four.\n---\n\nBody.\n"}),
        )
        .await;
    assert!(
        profile.skills().join("four/SKILL.md").is_file(),
        "applies directly"
    );
}

#[tokio::test]
async fn newly_copied_skill_becomes_invocable_after_reload() {
    let profile = Profile::new("");
    let session = Session::open(&profile).await;
    let model =
        Recording::wrap(mock_model(&profile, "[[rule]]\ntext = \"ok\"\nrepeat = true\n").await);
    let mut prompt = PromptSetup::new("cli", None);
    prompt.skills_index = session.skills.index_text();
    let agent = agent(&profile, &session, model.clone(), prompt);
    turn(&profile, &agent, session.id, "hello").await;

    write(
        &profile.skills(),
        "copied/SKILL.md",
        "---\nname: copied\ndescription: A copied skill.\n---\n\nCOPIED-BODY\n",
    );
    let changes = reload::reload(&session.skills);
    assert_eq!(changes.skills.added, ["copied"]);
    assert!(changes.render().contains("skills added: copied"));
    let expanded = dispatch::expand(&session.skills, "/copied go", &[])
        .await
        .unwrap()
        .unwrap();
    assert!(expanded.message.contains("COPIED-BODY"));
    turn(&profile, &agent, session.id, &expanded.message).await;
    let requests = model.requests();
    assert_eq!(
        system_text(&requests[0]),
        system_text(&requests[requests.len() - 1]),
        "the system prompt is unchanged"
    );
    assert!(reload::reload(&session.skills).is_empty());
}

const LEARN_SCRIPT: &str = r#"
[[rule]]
user = "The user ran /learn"
[[rule.tool_calls]]
name = "skill_manage"
arguments = { action = "create", name = "quickstart-api", category = "api", content = "---\nname: quickstart-api\ndescription: Call the example API with a token.\n---\n\n## When to Use\n\nCalling the example API.\n\n## Procedure\n\n1. Get a token.\n\n## Pitfalls\n\nTokens expire.\n\n## Verification\n\nA 200 reply.\n" }

[[rule]]
tool_result_regex = "Created skill"
text = "learned quickstart-api"
"#;

#[tokio::test]
async fn learn_writes_one_lint_clean_skill_through_skill_manage() {
    let profile = Profile::new("");
    let session = Session::open(&profile).await;
    let message = learn::message("https://docs.example.com/api/quickstart", 60).unwrap();
    assert!(message.contains("https://docs.example.com/api/quickstart"));
    assert!(message.contains("50000 characters") && message.contains("references/"));
    assert!(message.contains("patch it"));
    let model = mock_model(&profile, LEARN_SCRIPT).await;
    let agent = agent(&profile, &session, model, PromptSetup::new("cli", None));
    let result = turn(&profile, &agent, session.id, &message).await;
    assert_eq!(
        result.final_response.as_deref(),
        Some("learned quickstart-api")
    );
    let text =
        std::fs::read_to_string(profile.skills().join("api/quickstart-api").join(SKILL_FILE))
            .unwrap();
    let resolves = |_: &str| true;
    let findings = lint::lint(
        &text,
        &LintContext {
            dir_name: "quickstart-api",
            description_chars: 60,
            resolves: &resolves,
        },
    );
    assert!(findings.is_empty(), "{findings:?}");
}

const REFINE_SCRIPT: &str = r#"
[[rule]]
user = "focus on: save the deploy workflow as a skill"
[[rule.tool_calls]]
name = "skill_manage"
arguments = { action = "create", name = "deploy", category = "ops", content = "---\nname: deploy\ndescription: Deploy the service.\n---\n\n1. Build.\n2. Ship.\n" }

[[rule]]
tool_result_regex = "Created skill"
text = "saved the deploy workflow"
"#;

#[tokio::test]
async fn focused_refine_saves_a_workflow_and_leaves_the_session_alone() {
    let profile = Profile::new("");
    let parent = Session::open(&profile).await;
    let index_before = parent.skills.index_text();
    profile
        .store
        .append_message(
            parent.id,
            &NewMessage::new(Role::User, "deploy: build then ship"),
        )
        .unwrap();
    let child = review::child_session(&profile.store, parent.id).unwrap();
    let tools = Arc::new(SessionTools::new(
        None,
        Arc::new(dispatcher(&profile, &parent.sessions, &child.to_string()).await),
        Arc::new(Timings::default()),
    ));
    let run = |model, tools| ReviewRun {
        parent: Arc::clone(&parent.skills),
        session: parent.id,
        child,
        model,
        tools,
        sessions: Arc::clone(&parent.sessions),
        focus: Some("save the deploy workflow as a skill".to_owned()),
    };
    let held = parent.skills.begin_review().unwrap();
    let busy = refine::refine(run(
        mock_model(&profile, REFINE_SCRIPT).await,
        tools.clone(),
    ))
    .await
    .unwrap_err();
    assert_eq!(busy.message(), review::ALREADY_RUNNING);
    drop(held);

    let refined = refine::refine(run(mock_model(&profile, REFINE_SCRIPT).await, tools))
        .await
        .unwrap();
    assert!(
        refined.summary.contains("Created skills: deploy"),
        "{}",
        refined.summary
    );
    assert!(refined.summary.contains("saved the deploy workflow"));
    assert!(profile.skills().join("ops/deploy/SKILL.md").is_file());
    assert_eq!(
        parent.skills.index_text(),
        index_before,
        "session prompt untouched"
    );
    assert_eq!(
        profile
            .store
            .messages(parent.id, pmt_store::Visibility::Active)
            .unwrap()
            .len(),
        1,
        "session history untouched"
    );
}
