//! Verify-on-stop and the coding posture (spec: agent-turn-loop,
//! Verify-on-stop; Coding posture and standing coding instructions).

use std::fmt::Write as _;
use std::path::Path;

use jiff::Timestamp;
use pmt_agent::prompt::{self, PromptInputs};
use pmt_agent::turn::ExitReason;
use pmt_core::id::SessionId;
use pmt_providers::request::{Part, Role as W};
use pmt_store::Source;

use crate::support::{FakeTools, Profile, agent, mock_model};

/// `calls` as tool calls on step 0, a final answer on step 1 and another
/// on step 2 (after a nudge).
fn script(calls: &[(&str, &str)]) -> String {
    let mut text = String::from("[[rule]]\nstep = 0\n");
    for (name, args) in calls {
        let _ = write!(
            text,
            "[[rule.tool_calls]]\nname = \"{name}\"\narguments = {args}\n"
        );
    }
    text.push_str("\n[[rule]]\nstep = 1\ntext = \"Done.\"\n\n[[rule]]\nstep = 2\ntext = \"Tests cannot run here.\"\n");
    text
}

async fn run(
    config: &str,
    calls: &[(&str, &str)],
) -> (
    Profile,
    SessionId,
    Vec<pmt_providers::request::Request>,
    Option<String>,
) {
    let profile = Profile::new(config);
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, &script(calls), "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let result = run.turn(&profile, "fix the bug").await;
    assert_eq!(
        result.exit_reason,
        ExitReason::Completed,
        "{:?}",
        result.error
    );
    (profile, s, model.requests(), result.final_response)
}

const ON: &str = "[agent]\nverify_on_stop = true\n";

#[tokio::test]
async fn an_unverified_edit_is_nudged_and_the_turn_goes_on() {
    let (profile, s, requests, answer) = run(
        ON,
        &[("write_file", "{ path = \"src/lib.rs\", content = \"x\" }")],
    )
    .await;
    assert_eq!(
        requests.len(),
        3,
        "the final answer was not accepted at once"
    );
    let last = requests.last().unwrap().messages.last().unwrap().clone();
    assert_eq!(last.role, W::User);
    let Part::Text { text } = &last.parts[0] else {
        panic!("{last:?}")
    };
    assert!(text.contains("You changed src/lib.rs"), "{text}");
    assert!(
        text.contains("review the diff"),
        "verify_guidance is on by default"
    );
    assert_eq!(answer.as_deref(), Some("Tests cannot run here."));
    let ledger = profile.store.verify_ledger(s).unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(
        (ledger[0].1.as_str(), ledger[0].2.as_str()),
        ("edit", "src/lib.rs")
    );
}

#[tokio::test]
async fn docs_only_edits_pass_without_a_nudge() {
    let (_, _, requests, answer) = run(
        ON,
        &[("write_file", "{ path = \"README.md\", content = \"x\" }")],
    )
    .await;
    assert_eq!(requests.len(), 2);
    assert_eq!(answer.as_deref(), Some("Done."));
}

#[tokio::test]
async fn a_check_after_the_last_edit_is_evidence() {
    let (_, _, requests, _) = run(
        "[agent]\nverify_on_stop = true\nverify_guidance = false\n",
        &[
            ("write_file", "{ path = \"src/lib.rs\", content = \"x\" }"),
            ("terminal", "{ command = \"cargo test -p lib\" }"),
        ],
    )
    .await;
    assert_eq!(requests.len(), 2, "verified: no nudge");
}

#[tokio::test]
async fn off_by_default_and_no_ledger_then() {
    let (profile, s, requests, _) = run(
        "",
        &[("write_file", "{ path = \"src/lib.rs\", content = \"x\" }")],
    )
    .await;
    assert_eq!(requests.len(), 2);
    assert!(profile.store.verify_ledger(s).unwrap().is_empty());
}

fn inputs(profile: &Profile, platform: &str, cwd: &Path) -> PromptInputs {
    PromptInputs {
        config: profile.scope.config().clone(),
        profile_dir: profile.dir().to_path_buf(),
        platform: platform.into(),
        registered_hint: None,
        cwd: Some(cwd.to_path_buf()),
        cwd_fallback: false,
        no_context_files: false,
        model: "mock".into(),
        context_window: Some(128_000),
        tool_names: Vec::new(),
        caller_system: None,
        skills_index: None,
        pinned_skills: None,
        memory: None,
        plugin_sections: Vec::new(),
        session_id: SessionId::new(),
        pass_session_id: false,
        started_at: Timestamp::now(),
    }
}

#[test]
fn messaging_sessions_get_no_coding_brief() {
    let profile = Profile::new("[agent]\ncoding_instructions = \"Run just check.\"\n");
    let repo = profile.path("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let text = |platform: &str| {
        prompt::build(&inputs(&profile, platform, &repo))
            .unwrap()
            .prompt
            .sections()
            .iter()
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let telegram = text("telegram");
    assert!(!telegram.contains("# Coding"), "{telegram}");
    let cli = text("cli");
    assert!(cli.contains("# Coding"));
    assert!(cli.contains("Workspace: branch main"));
    assert!(cli.contains("Standing coding instructions:\nRun just check."));
}
