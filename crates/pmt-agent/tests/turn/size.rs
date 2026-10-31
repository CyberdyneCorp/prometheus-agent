//! The prompt size report measures exactly what a session sends (spec:
//! agent-turn-loop, Offline prompt size report).

use pmt_agent::prompt::size::{ToolSchema, report};
use pmt_agent::prompt::{self, PromptInputs};
use pmt_agent::turn::PromptSetup;
use pmt_store::Source;

use crate::support::{FakeTools, Profile, agent, mock_model};

#[tokio::test]
async fn the_report_sums_to_the_request_size() {
    let profile = Profile::new("[agent]\npersonality = \"teacher\"\n");
    std::fs::write(profile.dir().join("SOUL.md"), "You are Ada.").unwrap();
    let cwd = profile.path("repo");
    std::fs::write(cwd.join("AGENTS.md"), "Use tabs.").unwrap();
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let setup_prompt = PromptSetup::new("cli", Some(cwd.clone()));
    let started_at = setup_prompt.started_at;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |setup| {
        setup.prompt = setup_prompt;
    });
    run.turn(&profile, "hi").await;
    let request = model.requests().remove(0);

    // The same build a new session runs, measured offline.
    let built = prompt::build(&PromptInputs {
        config: profile.scope.config().clone(),
        profile_dir: profile.dir().to_path_buf(),
        platform: "cli".into(),
        registered_hint: None,
        cwd: Some(cwd),
        no_context_files: false,
        model: "mock".into(),
        context_window: Some(128_000),
        tool_names: request.tools.iter().map(|t| t.name.clone()).collect(),
        caller_system: None,
        skills_index: None,
        pinned_skills: None,
        memory: None,
        plugin_sections: Vec::new(),
        session_id: s,
        pass_session_id: false,
        started_at,
    })
    .unwrap();
    let tools_json = serde_json::to_string(&request.tools).unwrap();
    let schemas: Vec<ToolSchema> = request
        .tools
        .iter()
        .map(|t| ToolSchema {
            name: t.name.clone(),
            toolset: "lookup".into(),
            json: serde_json::to_string(t).unwrap(),
        })
        .collect();
    let report = report(&built.prompt, &schemas, &tools_json);

    let sent_system: usize = request.system.iter().map(|b| b.text.len()).sum();
    assert_eq!(report.system_bytes, sent_system);
    assert_eq!(
        report.layers.iter().map(|l| l.bytes).sum::<usize>(),
        sent_system
    );
    assert_eq!(report.tools_bytes, tools_json.len());
    assert_eq!(
        report.toolsets.iter().map(|t| t.bytes).sum::<usize>() + report.tools_framing_bytes,
        tools_json.len()
    );
    assert_eq!(report.total_bytes, sent_system + tools_json.len());
    let personality = report
        .layers
        .iter()
        .find(|l| l.layer == "personality")
        .unwrap();
    assert!(personality.bytes > 0 && personality.tier == "session");
}
