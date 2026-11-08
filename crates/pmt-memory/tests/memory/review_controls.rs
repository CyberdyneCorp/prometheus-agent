//! Background review controls (spec: memory, Background review controls):
//! the replayed-input budget stops a review before the call that would pass
//! it, every review call is recorded as `background_review`, and a review
//! borrows only extra tools its parent session offers.

use std::sync::Arc;

use pmt_agent::turn::model::{BoxFuture, RequestedCall};
use pmt_agent::turn::tools::{ToolBatch, ToolOutcome, ToolPort};
use pmt_core::cancel::CancelToken;
use pmt_memory::review::{ReviewRun, run};
use pmt_providers::request::Tool;
use pmt_store::{NewMessage, Role};
use pmt_tools::ToolResult;

use crate::support::{Profile, mock_model};

/// The parent session's tools: `lookup` and `notes`.
struct ParentTools;

impl ToolPort for ParentTools {
    fn definitions(&self) -> Vec<Tool> {
        ["lookup", "notes"]
            .iter()
            .map(|name| Tool {
                name: (*name).to_owned(),
                description: format!("the parent's {name}"),
                parameters: serde_json::json!({"type": "object"}),
            })
            .collect()
    }

    fn run<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        _: &'a str,
        _: &'a CancelToken,
    ) -> BoxFuture<'a, ToolBatch> {
        Box::pin(async move {
            ToolBatch {
                outcomes: calls
                    .iter()
                    .map(|c| ToolOutcome {
                        result: ToolResult::ok(format!("parent ran {}", c.name)),
                        duration_ms: 0,
                    })
                    .collect(),
                plan: None,
            }
        })
    }
}

const SAVE_THEN_ANSWER: &str = r#"
[[rule]]
times = 1
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "add", content = "Works in UTC." }] }

[[rule]]
text = "Saved."
"#;

fn review(
    profile: &Profile,
    model: Arc<dyn pmt_agent::turn::model::ModelPort>,
) -> (ReviewRun, pmt_core::id::SessionId) {
    let session = profile.session();
    let child = pmt_agent::review::child_session(&profile.store, session).unwrap();
    let run = ReviewRun {
        parent: profile.memory(session),
        session,
        child,
        model,
        focus: None,
        suggestions: None,
        parent_tools: Some(Arc::new(ParentTools)),
        digest: false,
    };
    (run, child)
}

/// Scenario: Input budget stops the review.
#[tokio::test]
async fn the_review_stops_before_the_call_that_would_pass_its_input_budget() {
    let profile = Profile::new("[memory.review]\nmax_input_tokens = 48000\n");
    let model = mock_model(&profile, SAVE_THEN_ANSWER).await;
    let (mut run_, child) = review(&profile, model.clone());
    // About 30,000 tokens of conversation: the first call fits, the
    // second (the same history plus the tool round) would bring the
    // replayed input past 48,000.
    profile
        .store
        .append_messages(
            run_.session,
            &[
                NewMessage::new(Role::User, "w ".repeat(60_000)),
                NewMessage::new(Role::Assistant, "ok"),
            ],
        )
        .unwrap();
    run_.parent_tools = None;
    let result = run(run_).await.unwrap();

    assert_eq!(model.requests().len(), 1, "the second call was never sent");
    assert_ne!(result.final_response.as_deref(), Some("Saved."));
    let calls = profile.store.usage_events(child).unwrap();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].purpose, "background_review");
}

/// Scenario: Extra tool must exist on the parent.
#[tokio::test]
async fn an_extra_tool_the_parent_lacks_is_not_offered() {
    let profile =
        Profile::new("[memory.review]\nextra_tools = [\"propose_shared_memory\", \"lookup\"]\n");
    let model = mock_model(&profile, "[[rule]]\ntext = \"nothing to save\"\n").await;
    let (run_, _) = review(&profile, model.clone());
    run(run_).await.unwrap();

    let tools: Vec<String> = model.requests()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert!(
        !tools.contains(&"propose_shared_memory".to_owned()),
        "{tools:?}"
    );
    assert!(
        tools.contains(&"lookup".to_owned()),
        "a parent tool is borrowed: {tools:?}"
    );
    assert!(
        !tools.contains(&"notes".to_owned()),
        "only listed tools: {tools:?}"
    );
}
