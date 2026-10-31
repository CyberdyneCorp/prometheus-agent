//! Interrupts and steering (spec: agent-turn-loop, Interrupts; Steer
//! placement preserves the cache prefix).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pmt_agent::turn::model::{BoxFuture, DeltaSink, RequestedCall};
use pmt_agent::turn::tools::{ToolBatch, ToolOutcome, ToolPort};
use pmt_agent::turn::{ExitReason, Interrupt, Issuer, TurnInput};
use pmt_core::cancel::CancelToken;
use pmt_providers::request::{Part, Role as W, Tool};
use pmt_store::{Role, Source, Visibility};
use pmt_tools::{Status, ToolResult};
use serde_json::json;

use crate::support::{FakeTools, Profile, agent, mock_model, options};

/// One tool call, then an answer.
const TOOL_THEN_ANSWER: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "x" }

[[rule]]
step = 1
text = "Done."
"#;

/// Stops the turn when the first answer text arrives.
struct StopOnFirstDelta {
    interrupt: Interrupt,
    seen: Mutex<String>,
}

impl DeltaSink for StopOnFirstDelta {
    fn text(&self, text: &str) {
        self.seen.lock().unwrap().push_str(text);
        self.interrupt.stop(Issuer::User);
    }
}

fn rows(profile: &Profile, s: pmt_core::id::SessionId) -> Vec<(Role, String)> {
    profile
        .store
        .messages(s, Visibility::Active)
        .unwrap()
        .into_iter()
        .map(|m| (m.role, m.content.unwrap_or_default()))
        .collect()
}

#[tokio::test]
async fn an_interrupt_mid_stream_ends_the_turn_cleanly() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let script = "latency_ms = 100\n[[rule]]\nstep = 0\ntext = \"one two three four five six\"\n\n[[rule]]\nstep = 1\ntext = \"Next answer.\"\n";
    let model = mock_model(&profile, script, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let mut opts = options();
    let sink = Arc::new(StopOnFirstDelta {
        interrupt: opts.interrupt.clone(),
        seen: Mutex::new(String::new()),
    });
    opts.deltas = sink.clone();
    let result = run.turn_with(&profile, TurnInput::text("talk"), opts).await;

    assert_eq!(result.exit_reason, ExitReason::InterruptedByUser);
    assert_eq!(*sink.seen.lock().unwrap(), "one ", "the stream stopped");
    assert_eq!(run.end_reasons(), ["interrupted_by_user"]);
    // No partial answer is stored; a boundary row closes the turn.
    let stored = rows(&profile, s);
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1].0, Role::Assistant);
    assert!(
        stored[1]
            .1
            .starts_with("[prometheus] The turn ended (interrupted_by_user)"),
        "{}",
        stored[1].1
    );
    // The lease was released: the next turn runs.
    let next = run.turn(&profile, "again").await;
    assert_eq!(next.exit_reason, ExitReason::Completed);
}

#[tokio::test]
async fn a_steer_is_a_new_row_after_the_last_tool_result() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, TOOL_THEN_ANSWER, "mock").await;
    let tools = Arc::new(FakeTools {
        delay: Duration::from_millis(300),
        ..FakeTools::plain()
    });
    let run = agent(&profile, s, model.clone(), tools, |_| {});
    let steer = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        run.agent.steer("use the retry helper").unwrap();
        run.agent.steer("and log it").unwrap();
    };
    let (result, ()) = tokio::join!(run.turn(&profile, "fix"), steer);
    assert_eq!(result.exit_reason, ExitReason::Completed);

    let requests = model.requests();
    let second = &requests[1];
    let roles: Vec<_> = second.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [W::User, W::Assistant, W::Tool, W::User]);
    assert_eq!(
        second.messages[3].parts,
        [Part::text(
            "[OUT-OF-BAND USER MESSAGE kind=note]\nuse the retry helper\n\nand log it\n[/OUT-OF-BAND USER MESSAGE]"
        )]
    );
    assert!(
        matches!(&second.messages[2].parts[0], Part::ToolResult { content, .. } if content == "result of {\"q\":\"x\"}"),
        "the tool result is unchanged"
    );
}

#[tokio::test]
async fn a_steer_during_a_final_answer_opens_the_next_user_message() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let script = "[[rule]]\nstep = 0\ntext = \"Answer.\"\nlatency_ms = 150\n\n[[rule]]\nstep = 1\ntext = \"Second.\"\n";
    let model = mock_model(&profile, script, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let steer = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        run.agent.steer("prefer tabs").unwrap();
    };
    let (first, ()) = tokio::join!(run.turn(&profile, "hi"), steer);
    assert_eq!(first.exit_reason, ExitReason::Completed);
    assert_eq!(rows(&profile, s).len(), 2, "nothing was added to sent rows");

    run.turn(&profile, "next").await;
    let stored = rows(&profile, s);
    assert_eq!(
        stored[2].1,
        "[OUT-OF-BAND USER MESSAGE kind=note]\nprefer tabs\n[/OUT-OF-BAND USER MESSAGE]\n\nnext"
    );
}

#[tokio::test]
async fn an_empty_steer_is_rejected() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, TOOL_THEN_ANSWER, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let err = run.agent.steer(" \n\t").unwrap_err();
    assert_eq!(err.message(), "empty steer");
    assert_eq!(err.code().code, "turn.empty_steer");
}

/// Stops the turn (soft or hard) and steers while the tool batch runs.
async fn stop_during_tools(
    hard: bool,
) -> (
    Profile,
    pmt_core::id::SessionId,
    Vec<(Role, String)>,
    ExitReason,
) {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let script = format!("{TOOL_THEN_ANSWER}\n[[rule]]\nstep = 2\ntext = \"Third.\"\n");
    let model = mock_model(&profile, &script, "mock").await;
    let tools = Arc::new(FakeTools {
        delay: Duration::from_millis(400),
        ..FakeTools::plain()
    });
    let run = agent(&profile, s, model, tools, |_| {});
    let opts = options();
    let interrupt = opts.interrupt.clone();
    let stop = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        run.agent.steer("keep it short").unwrap();
        if hard {
            interrupt.stop(Issuer::User);
        } else {
            interrupt.stop_soft(Issuer::User);
        }
    };
    let (result, ()) = tokio::join!(run.turn_with(&profile, TurnInput::text("go"), opts), stop);
    let reason = result.exit_reason;
    let after = rows(&profile, s);
    run.turn(&profile, "continue").await;
    let mut all = rows(&profile, s);
    let next_user = all.drain(after.len()..).next().unwrap();
    let mut out = after;
    out.push(next_user);
    (profile, s, out, reason)
}

#[tokio::test]
async fn a_soft_stop_lets_the_batch_finish_and_keeps_the_steer() {
    let (_profile, _s, rows, reason) = stop_during_tools(false).await;
    assert_eq!(reason, ExitReason::InterruptedByUser);
    let tool = rows.iter().find(|(role, _)| *role == Role::Tool).unwrap();
    assert_eq!(
        tool.1, "result of {\"q\":\"x\"}",
        "the running batch finished"
    );
    let next = rows.last().unwrap();
    assert!(
        next.1.contains("keep it short") && next.1.ends_with("continue"),
        "{}",
        next.1
    );
}

#[tokio::test]
async fn a_hard_stop_cancels_tools_and_discards_the_steer() {
    let (_profile, _s, rows, reason) = stop_during_tools(true).await;
    assert_eq!(reason, ExitReason::InterruptedByUser);
    assert!(rows.iter().all(|(role, _)| *role != Role::Tool), "{rows:?}");
    assert_eq!(rows.last().unwrap().1, "continue");
}

/// A tool standing in for a delegated child: the child runs as its own
/// task under a child of the turn's cancel token (as delegation passes it)
/// until that token fires.
#[derive(Default)]
struct Child {
    cancelled: Arc<AtomicBool>,
}

impl ToolPort for Child {
    fn definitions(&self) -> Vec<Tool> {
        vec![Tool {
            name: "lookup".into(),
            description: "Delegate.".into(),
            parameters: json!({"type": "object"}),
        }]
    }

    fn run<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, ToolBatch> {
        let token = cancel.child();
        let flag = self.cancelled.clone();
        let child = tokio::spawn(async move {
            token.cancelled().await;
            flag.store(true, Ordering::SeqCst);
        });
        Box::pin(async move {
            let _ = child.await;
            ToolBatch {
                outcomes: calls
                    .iter()
                    .map(|_| ToolOutcome {
                        result: ToolResult::text(Status::Cancelled, "stopped"),
                        duration_ms: 0,
                    })
                    .collect(),
                plan: None,
            }
        })
    }
}

#[tokio::test]
async fn stopping_a_turn_interrupts_its_running_child() {
    let profile = Profile::new("");
    let s = profile.session(Source::Tui);
    let model = mock_model(&profile, TOOL_THEN_ANSWER, "mock").await;
    let child = Arc::new(Child::default());
    let run = agent(&profile, s, model, child.clone(), |_| {});
    let opts = options();
    let interrupt = opts.interrupt.clone();
    let stop = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        interrupt.stop(Issuer::User);
    };
    let started = Instant::now();
    let (result, ()) = tokio::join!(
        run.turn_with(&profile, TurnInput::text("delegate"), opts),
        stop
    );
    assert_eq!(result.exit_reason, ExitReason::InterruptedByUser);
    assert!(started.elapsed() < Duration::from_secs(2));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        child.cancelled.load(Ordering::SeqCst),
        "the child saw the stop"
    );
}

#[tokio::test]
async fn interrupting_one_session_leaves_another_running() {
    let profile = Profile::new("");
    let (a, b) = (
        profile.session(Source::Gateway),
        profile.session(Source::Gateway),
    );
    let slow = || {
        Arc::new(FakeTools {
            delay: Duration::from_millis(300),
            ..FakeTools::plain()
        })
    };
    let run_a = agent(
        &profile,
        a,
        mock_model(&profile, TOOL_THEN_ANSWER, "mock").await,
        slow(),
        |_| {},
    );
    let run_b = agent(
        &profile,
        b,
        mock_model(&profile, TOOL_THEN_ANSWER, "mock").await,
        slow(),
        |_| {},
    );
    let (opts_a, opts_b) = (options(), options());
    let stop_a = opts_a.interrupt.clone();
    let stop = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop_a.stop(Issuer::User);
    };
    let (ra, rb, ()) = tokio::join!(
        run_a.turn_with(&profile, TurnInput::text("a"), opts_a),
        run_b.turn_with(&profile, TurnInput::text("b"), opts_b),
        stop
    );
    assert_eq!(ra.exit_reason, ExitReason::InterruptedByUser);
    assert_eq!(rb.exit_reason, ExitReason::Completed);
}
