//! 17.3: the skills index in the system prompt, frozen for the session;
//! changes reach the model as a per-turn note, `--now` rebuilds once.

use std::sync::Arc;

use pmt_agent::prompt::frozen::Rebuild;
use pmt_agent::turn::{Agent, AgentSetup, PromptSetup, Recorded, TurnInput, TurnOptions};
use pmt_core::id::SessionId;
use pmt_obs::trajectory::events::Event;
use pmt_skills::activation::{self, DEFERRED};
use pmt_skills::index::{NOTE_PREFIX, Posture};
use pmt_store::{Role, Visibility};

use crate::support::{Profile, Session, add_skill, mock_model, skill_md, write};

const SCRIPT: &str = r#"
[[rule]]
user = "make a skill"
[[rule.tool_calls]]
name = "skill_manage"
arguments = { action = "create", name = "new-thing", content = "---\nname: new-thing\ndescription: Do the new thing.\n---\n\nSteps.\n" }

[[rule]]
tool_result_regex = "Created skill"
text = "created"

[[rule]]
repeat = true
text = "ok"
"#;

/// An agent over the mock model whose prompt takes the session's frozen
/// skills index.
async fn agent(profile: &Profile, session: &Session, events: Arc<Recorded>) -> Agent {
    let model = mock_model(profile, SCRIPT).await;
    let mut prompt = PromptSetup::new("cli", None);
    prompt.skills_index = session.skills.index_text();
    let mut setup = AgentSetup::new(
        profile.scope.clone(),
        Arc::clone(&profile.store),
        session.id,
        model,
        session.tool_port(),
        prompt,
    );
    let skills = Arc::clone(&session.skills);
    setup.refresh_prompt = Some(Arc::new(move |p: &mut PromptSetup| {
        p.skills_index = skills.index_text();
    }));
    setup.events = events;
    Agent::new(setup).unwrap()
}

async fn turn(profile: &Profile, agent: &Agent, session: &Session, text: &str) {
    let mut input = TurnInput::text(text);
    input.additions.extend(session.skills.change_note());
    agent
        .run_turn(
            Some(&profile.scope),
            session.id,
            input,
            TurnOptions::default(),
        )
        .await
        .unwrap();
}

fn prompt_hash(profile: &Profile, id: SessionId) -> String {
    profile.store.session_prompt(id).unwrap().unwrap().sha256
}

fn prompt_text(profile: &Profile, id: SessionId) -> String {
    let stored = profile.store.session_prompt(id).unwrap().unwrap();
    stored
        .tiers
        .iter()
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn last_user_text(profile: &Profile, id: SessionId) -> String {
    let rows = profile.store.messages(id, Visibility::Active).unwrap();
    rows.iter()
        .rev()
        .find(|r| r.role == Role::User)
        .unwrap()
        .content
        .clone()
        .unwrap()
}

fn rebuild_reasons(events: &Recorded) -> Vec<String> {
    events
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::PromptRebuilt(p) => Some(p.reason),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn long_description_is_clipped_and_grouped_by_category() {
    let profile = Profile::new("");
    let long = "Analyze large CSV exports, join them with the warehouse tables and produce weekly revenue reports for the finance team with charts and note.";
    assert_eq!(long.chars().count(), 140);
    add_skill(&profile.skills(), "data/csv-report", "csv-report", long);
    add_skill(
        &profile.skills(),
        "data/a-first",
        "a-first",
        "Sorted first.",
    );
    add_skill(&profile.skills(), "creative/poems", "poems", "Write poems.");
    let session = Session::open(&profile).await;
    let index = session.skills.index_text().unwrap();
    assert!(index.starts_with("## Skills\n"), "{index}");
    assert!(index.contains("<available_skills>") && index.ends_with("</available_skills>"));
    let row = index
        .lines()
        .find(|l| l.starts_with("- csv-report: "))
        .unwrap();
    let description = row.trim_start_matches("- csv-report: ");
    assert!(description.chars().count() <= 60, "{description}");
    assert!(long.starts_with(description));
    assert!(
        long[description.len()..].starts_with([' ', ',']),
        "not at a word boundary: {description}"
    );
    let creative = index.find("creative:").unwrap();
    let data = index.find("data:").unwrap();
    assert!(creative < data);
    assert!(index.find("- a-first").unwrap() < index.find("- csv-report").unwrap());
}

#[tokio::test]
async fn coding_posture_demotes_and_safe_worker_has_no_index() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "creative/poems", "poems", "Write poems.");
    add_skill(&profile.skills(), "creative/songs", "songs", "Write songs.");
    add_skill(&profile.skills(), "dev/git", "git", "Use git.");
    let mut coding = profile.setup(profile.session());
    coding.posture = Posture::Coding;
    let session = Session::start(&profile, coding).await;
    let index = session.skills.index_text().unwrap();
    assert!(
        index.contains("creative [names only]: poems, songs"),
        "{index}"
    );
    assert!(index.contains("- git: Use git."));
    let mut safe = profile.setup(profile.session());
    safe.posture = Posture::SafeWorker;
    let session = Session::start(&profile, safe).await;
    assert_eq!(session.skills.index_text(), None);
}

#[tokio::test]
async fn installing_a_skill_mid_session_waits_for_the_next_session() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "dev/git", "git", "Use git.");
    let session = Session::open(&profile).await;
    let events = Recorded::new();
    let agent = agent(&profile, &session, Arc::clone(&events)).await;
    turn(&profile, &agent, &session, "hello").await;
    let hash = prompt_hash(&profile, session.id);
    // A skill lands on disk mid-session (an install).
    add_skill(
        &profile.skills(),
        "ops/terraform",
        "terraform",
        "Plan and apply Terraform.",
    );
    turn(&profile, &agent, &session, "next").await;
    assert_eq!(prompt_hash(&profile, session.id), hash);
    assert!(!prompt_text(&profile, session.id).contains("terraform"));
    let user = last_user_text(&profile, session.id);
    assert!(
        user.contains(NOTE_PREFIX) && user.contains("terraform: Plan and apply Terraform."),
        "{user}"
    );
    // The same difference is not repeated.
    turn(&profile, &agent, &session, "again").await;
    assert!(!last_user_text(&profile, session.id).contains(NOTE_PREFIX));
    assert_eq!(rebuild_reasons(&events), ["new_session"]);
    // The next session's index has it.
    let next = Session::open(&profile).await;
    assert!(
        next.skills
            .index_text()
            .unwrap()
            .contains("- terraform: Plan and apply Terraform.")
    );
}

#[tokio::test]
async fn agent_created_skill_does_not_rebuild_the_prompt() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "dev/git", "git", "Use git.");
    let session = Session::open(&profile).await;
    let events = Recorded::new();
    let agent = agent(&profile, &session, Arc::clone(&events)).await;
    turn(&profile, &agent, &session, "hello").await;
    let hash = prompt_hash(&profile, session.id);
    turn(&profile, &agent, &session, "make a skill").await;
    assert!(profile.skills().join("new-thing/SKILL.md").is_file());
    turn(&profile, &agent, &session, "thanks").await;
    assert_eq!(prompt_hash(&profile, session.id), hash);
    let user = last_user_text(&profile, session.id);
    assert!(user.starts_with("thanks"));
    assert!(
        user.contains(NOTE_PREFIX) && user.contains("new-thing"),
        "{user}"
    );
    assert_eq!(rebuild_reasons(&events), ["new_session"]);
}

#[tokio::test]
async fn skills_change_is_deferred_unless_now() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "dev/git", "git", "Use git.");
    add_skill(
        &profile.skills(),
        "ops/terraform",
        "terraform",
        "Plan and apply Terraform.",
    );
    let session = Session::open(&profile).await;
    let events = Recorded::new();
    let agent = agent(&profile, &session, Arc::clone(&events)).await;
    turn(&profile, &agent, &session, "hello").await;
    let hash = prompt_hash(&profile, session.id);
    let reply = activation::run(&session.skills, "disable terraform", &[]).unwrap();
    assert!(
        reply.text.ends_with(DEFERRED) && !reply.rebuild_now,
        "{}",
        reply.text
    );
    turn(&profile, &agent, &session, "next").await;
    assert_eq!(prompt_hash(&profile, session.id), hash);
    assert!(last_user_text(&profile, session.id).contains("Removed: terraform."));
    let reply = activation::run(&session.skills, "enable terraform --now", &[]).unwrap();
    assert!(reply.rebuild_now);
    agent.rebuild_prompt(Rebuild::Now("skills".to_owned()));
    turn(&profile, &agent, &session, "after").await;
    assert_eq!(rebuild_reasons(&events), ["new_session", "skills --now"]);
    assert!(prompt_text(&profile, session.id).contains("- terraform:"));
    assert!(!last_user_text(&profile, session.id).contains(NOTE_PREFIX));
}

#[tokio::test]
async fn hub_install_is_m2() {
    let profile = Profile::new("");
    write(&profile.skills(), "x/SKILL.md", &skill_md("x", "X.", ""));
    let session = Session::open(&profile).await;
    let err = activation::run(
        &session.skills,
        "install official/devops/terraform --now",
        &[],
    )
    .unwrap_err();
    assert_eq!(err.code().code, "skill.hub_unavailable");
}
