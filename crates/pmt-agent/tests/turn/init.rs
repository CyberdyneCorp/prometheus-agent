//! `/init` (spec: agent-turn-loop, Project instructions bootstrap with
//! /init).

use std::sync::Arc;

use pmt_agent::turn::ExitReason;
use pmt_providers::request::Part;
use pmt_store::Source;

use crate::support::{FakeTools, Profile, agent, mock_model, options};

const WRITES: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "AGENTS.md", content = "rewritten" }

[[rule]]
step = 1
text = "AGENTS.md updated."
"#;

#[tokio::test]
async fn an_existing_agents_file_keeps_the_users_sections() {
    let profile = Profile::new("");
    let repo = profile.path("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let agents = repo.join("AGENTS.md");
    let release = "## Release rules\n\nTag releases from main only.\nNever force-push a tag.";
    std::fs::write(&agents, format!("# Project\n\n{release}\n")).unwrap();
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, WRITES, "mock").await;
    // The model's write drops the user's section.
    let target = agents.clone();
    let tools = Arc::new(FakeTools {
        effect: Some(Box::new(move || {
            std::fs::write(
                &target,
                "# Project\n\n## Testing\n\nRun cargo nextest run.\n",
            )
            .unwrap();
        })),
        ..FakeTools::plain()
    });
    let cwd = repo.clone();
    let run = agent(&profile, s, model.clone(), tools, |setup| {
        setup.prompt.cwd = Some(cwd);
    });
    let result = run
        .turn_with(
            &profile,
            pmt_agent::init::input("focus on the test commands"),
            options(),
        )
        .await;
    assert_eq!(result.exit_reason, ExitReason::Completed);

    let text = std::fs::read_to_string(&agents).unwrap();
    assert!(text.contains(release), "kept verbatim: {text}");
    assert!(text.contains("## Testing"), "the update stays: {text}");
    let requests = model.requests();
    let first = &requests[0];
    let Part::Text { text: sent } = &first.messages[0].parts[0] else {
        panic!()
    };
    assert!(sent.ends_with("Pay particular attention to: focus on the test commands"));
    assert_eq!(
        first.system, requests[1].system,
        "the new file is context from the next session only"
    );
}
