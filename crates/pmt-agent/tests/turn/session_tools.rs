//! The production tool port: agent-level tools run in the loop, every
//! other call goes through the session's dispatcher (spec: tool-system,
//! Agent-level tools are owned by the turn loop; agent-turn-loop,
//! Iteration phases).

use std::sync::Arc;
use std::time::Duration;

use pmt_agent::tools::{AgentTools, Clarifier, SessionSearch};
use pmt_agent::turn::{ExitReason, SessionTools, Timings};
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_core::panic::CrashReporter;
use pmt_core::secret::Redactor;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::BatchPlanned;
use pmt_store::{Source, Visibility};
use pmt_tools::budget::ResultBudget;
use pmt_tools::dispatch::{AllowAll, NoHooks};
use pmt_tools::probe::ProbeCache;
use pmt_tools::{
    Builtins, DispatchSetup, Dispatcher, Selection, SurfaceInputs, ToolRegistry, ToolSurface,
};

use crate::support::{Profile, agent, mock_model};

const SCRIPT: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "todo"
arguments = { todos = [{ id = "a", content = "write tests", status = "in_progress" }] }
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "missing.txt" }

[[rule]]
step = 1
text = "Planned."
"#;

async fn session_tools(profile: &Profile, session: pmt_core::id::SessionId) -> Arc<SessionTools> {
    let config = profile.scope.config();
    let environments = pmt_tools::environments::Environments::process();
    let registry = ToolRegistry::new(
        "default",
        Arc::new(Builtins::standard(pmt_tools::builtin_tools(&environments)).unwrap()),
    );
    let selection = Selection::resolve(
        &registry,
        &["file".to_owned(), "todo".to_owned(), "clarify".to_owned()],
        &[],
    )
    .unwrap();
    let capabilities = CapabilitySet::parse(BASELINE.iter().copied()).unwrap();
    let surface = ToolSurface::build(SurfaceInputs {
        registry: &registry,
        selection: &selection,
        capabilities: &capabilities,
        config,
        probes: &ProbeCache::new(),
    })
    .await;
    let timings = Arc::new(Timings::default());
    let dispatcher = Dispatcher::new(DispatchSetup {
        session_id: session.to_string(),
        surface: Arc::new(surface),
        capabilities: Arc::new(capabilities),
        hooks: Arc::new(NoHooks),
        gate: Arc::new(AllowAll),
        observer: timings.clone(),
        budget: ResultBudget::new(
            config.tools.result_budget.clone(),
            None,
            profile.dir().join("spill"),
            Redactor::default(),
        ),
        reporter: Arc::new(CrashReporter::new(
            profile.dir().join("crash"),
            Redactor::default(),
        )),
        default_timeout: Duration::from_secs(30),
        concurrency: 4,
    });
    let agent_tools = AgentTools::new(
        profile.store.clone(),
        session,
        Clarifier::unattended(),
        SessionSearch::new(profile.store.clone(), session),
    )
    .unwrap();
    Arc::new(SessionTools::new(
        Some(Arc::new(agent_tools)),
        Arc::new(dispatcher),
        timings,
    ))
}

#[tokio::test]
async fn agent_level_and_dispatched_calls_answer_in_call_order() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, SCRIPT, "mock").await;
    let tools = session_tools(&profile, s).await;
    let run = agent(&profile, s, model.clone(), tools, |_| {});
    let result = run.turn(&profile, "plan").await;

    assert_eq!(result.exit_reason, ExitReason::Completed);
    let todos = profile.store.todos(s).unwrap().unwrap();
    assert_eq!(todos.items[0]["content"], "write tests");
    let rows = profile.store.messages(s, Visibility::Active).unwrap();
    let tool_rows: Vec<_> = rows.iter().filter(|m| m.tool_name.is_some()).collect();
    assert_eq!(tool_rows[0].tool_name.as_deref(), Some("todo"));
    assert_eq!(tool_rows[1].tool_name.as_deref(), Some("read_file"));
    assert_eq!(tool_rows[1].metadata.as_ref().unwrap()["is_error"], true);
    // The frozen tool list is the dispatcher's surface.
    let names: Vec<String> = model.requests()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert!(names.contains(&"todo".to_owned()) && names.contains(&"read_file".to_owned()));
}

const BATCH: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "a.txt" }
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "b.txt" }
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "out.txt", content = "1" }
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "./out.txt", content = "2" }
[[rule.tool_calls]]
name = "clarify"
arguments = { questions = [{ question = "Which?" }] }
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "c.txt" }

[[rule]]
step = 1
text = "Done."
"#;

#[tokio::test]
async fn the_batch_plan_is_recorded_with_its_dependency_edges() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, BATCH, "mock").await;
    let tools = session_tools(&profile, s).await;
    let run = agent(&profile, s, model, tools, |_| {});
    let result = run.turn(&profile, "go").await;
    assert_eq!(result.exit_reason, ExitReason::Completed);

    let plans: Vec<BatchPlanned> = run
        .events
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::ToolBatchPlanned(plan) => Some(plan),
            _ => None,
        })
        .collect();
    assert_eq!(plans.len(), 1);
    let ids: Vec<String> = (0..6).map(|i| format!("call_0_{i}")).collect();
    assert_eq!(plans[0].calls, ids);
    let edges: Vec<(String, String)> = plans[0]
        .edges
        .iter()
        .map(|e| (e.after.clone(), e.call.clone()))
        .collect();
    let edge = |from: usize, to: usize| (ids[from].clone(), ids[to].clone());
    // The reads wait for nothing; the second write waits for the first;
    // `clarify` (interactive, run by the agent loop) is a barrier.
    assert_eq!(
        edges,
        [
            edge(2, 3),
            edge(0, 4),
            edge(1, 4),
            edge(2, 4),
            edge(3, 4),
            edge(4, 5),
        ]
    );
    let rows = profile.store.messages(s, Visibility::Active).unwrap();
    let clarify = rows
        .iter()
        .find(|m| m.tool_name.as_deref() == Some("clarify"))
        .unwrap();
    assert!(
        !clarify
            .content
            .as_deref()
            .unwrap()
            .contains("must be handled by the agent loop"),
        "clarify ran in the agent loop"
    );
}
