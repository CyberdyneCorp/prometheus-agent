//! Subdirectory hints (spec: agent-turn-loop, Subdirectory hints ride tool
//! results).

use std::path::Path;

use pmt_providers::request::{Part, Request, Role as W};
use pmt_store::Source;

use crate::support::{FakeTools, Profile, agent, mock_model};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Reads `path` on steps 0 and 1, then answers.
fn reads(path: &str) -> String {
    format!(
        "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"read_file\"\narguments = {{ path = \"{path}\" }}\n\n\
         [[rule]]\nstep = 1\n[[rule.tool_calls]]\nname = \"read_file\"\narguments = {{ path = \"{path}\" }}\n\n\
         [[rule]]\nstep = 2\ntext = \"Done.\"\n"
    )
}

fn tool_texts(request: &Request) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|m| m.role == W::Tool)
        .map(|m| match &m.parts[0] {
            Part::ToolResult { content, .. } => content.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn reading_a_subpackage_appends_its_agents_file_to_that_result_once() {
    let profile = Profile::new("");
    let repo = profile.path("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write(&repo.join("AGENTS.md"), "Root rules.");
    write(
        &repo.join("pkg/sub/AGENTS.md"),
        "Sub rules: run cargo test -p sub.",
    );
    write(&repo.join("pkg/sub/lib.rs"), "fn main() {}");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, &reads("pkg/sub/lib.rs"), "mock").await;
    let cwd = repo.clone();
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |setup| {
        setup.prompt.cwd = Some(cwd);
    });
    run.turn(&profile, "look").await;

    let last = model.requests().pop().unwrap();
    let results = tool_texts(&last);
    assert_eq!(results.len(), 2);
    assert!(
        results[0].ends_with(
            "[Subdirectory context: pkg/sub/AGENTS.md]\nSub rules: run cargo test -p sub."
        ),
        "{}",
        results[0]
    );
    assert!(
        !results[1].contains("Subdirectory context"),
        "once per file"
    );
    assert!(
        last.system.iter().all(|b| !b.text.contains("Sub rules")),
        "never in the system prompt"
    );
    assert!(
        results.iter().all(|r| !r.contains("Root rules")),
        "the project context already holds the root file"
    );

    // A new process on the same session does not add it again.
    let model = mock_model(&profile, &reads("pkg/sub/lib.rs"), "mock").await;
    let cwd = repo.clone();
    let again = agent(&profile, s, model.clone(), FakeTools::new(), |setup| {
        setup.prompt.cwd = Some(cwd);
    });
    again.turn(&profile, "look again").await;
    let last = model.requests().pop().unwrap();
    let hinted = tool_texts(&last)
        .iter()
        .filter(|r| r.contains("[Subdirectory context: pkg/sub/AGENTS.md]"))
        .count();
    assert_eq!(hinted, 1, "only the first turn's row carries it");
}

#[tokio::test]
async fn paths_outside_the_working_tree_bring_no_hint() {
    let profile = Profile::new("");
    let repo = profile.path("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let outside = profile.path("elsewhere");
    write(&outside.join("AGENTS.md"), "Not this project.");
    write(&outside.join("x.rs"), "");
    let s = profile.session(Source::Cli);
    let model = mock_model(
        &profile,
        &reads(&outside.join("x.rs").display().to_string()),
        "mock",
    )
    .await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |setup| {
        setup.prompt.cwd = Some(repo);
    });
    run.turn(&profile, "look").await;
    let last = model.requests().pop().unwrap();
    assert!(
        tool_texts(&last)
            .iter()
            .all(|r| !r.contains("Subdirectory context"))
    );
}
