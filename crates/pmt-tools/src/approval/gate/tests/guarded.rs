//! Smart approvals with a denial breaker and approval explain (spec:
//! tool-system, Smart approvals with a denial breaker; Approval explain and
//! dry-run), through the real dispatcher with a scripted guardian.

use std::sync::Mutex;

use super::*;
use crate::approval::guardian::{Guardian, GuardianVerdict};

/// Answers from a script; an exhausted script fails (escalates).
#[derive(Debug)]
struct Scripted(Mutex<Vec<Result<GuardianVerdict, String>>>);

impl Scripted {
    fn new(answers: Vec<Result<GuardianVerdict, String>>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(answers)))
    }
}

impl Guardian for Scripted {
    fn judge<'a>(
        &'a self,
        _: &'a ApprovalRequest,
    ) -> SeamFuture<'a, Result<GuardianVerdict, String>> {
        let mut answers = self.0.lock().unwrap();
        let answer = if answers.is_empty() {
            Err("no answer".to_owned())
        } else {
            answers.remove(0)
        };
        Box::pin(async move { answer })
    }
}

fn smart(guardian: Arc<Scripted>) -> Options {
    Options {
        approvals: ApprovalsConfig {
            mode: ApprovalMode::Smart,
            ..ApprovalsConfig::default()
        },
        guardian: Some(guardian),
        ..Options::default()
    }
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "one guardian answer, as the script holds them"
)]
fn deny() -> Result<GuardianVerdict, String> {
    Ok(GuardianVerdict::Deny("destroys build output".into()))
}

/// Scenario: Third denial.
#[tokio::test]
async fn the_third_consecutive_guardian_denial_trips_the_breaker() {
    let fx = fixture();
    let (channel, _prompts) = attached();
    let session = session(
        &fx,
        channel,
        smart(Scripted::new(vec![deny(), deny(), deny(), deny()])),
    )
    .await;
    for (n, line) in ["rm -rf build", "rm -rf ./build", "rm -r -f build"]
        .into_iter()
        .enumerate()
    {
        let result = command(&session, line).await;
        assert_eq!(result.status, Status::Denied, "{line}");
        let text = result.text_content();
        assert!(text.contains("approval guardian refused it"), "{text}");
        assert_eq!(text.contains("CIRCUIT BREAKER"), n == 2, "{text}");
    }
    let fourth = command(&session, "rm -rf build2").await;
    assert!(
        fourth
            .text_content()
            .contains("CIRCUIT BREAKER: 4 consecutive commands were blocked. STOP attempting variations of this operation."),
        "{}",
        fourth.text_content()
    );
    assert_eq!(fx.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_human_approval_resets_the_count_and_errors_escalate() {
    let fx = fixture();
    let (channel, mut prompts) = attached();
    let guardian = Scripted::new(vec![
        deny(),
        deny(),
        Err("timed out".into()),
        deny(),
        deny(),
        Ok(GuardianVerdict::Approve),
    ]);
    let session = session(&fx, channel, smart(guardian)).await;
    command(&session, "rm -rf a").await;
    command(&session, "rm -rf b").await;
    // The guardian failed: the human is asked, and approves.
    let asked = run(&session, "terminal", json!({"command": "rm -rf c"}));
    answer(&mut prompts, Decision::Once).await;
    assert_eq!(asked.await.unwrap().status, Status::Ok);
    command(&session, "rm -rf d").await;
    let second = command(&session, "rm -rf e").await;
    assert!(!second.text_content().contains("CIRCUIT BREAKER"));
    // The guardian approves without asking anyone.
    assert_eq!(command(&session, "rm -rf f").await.status, Status::Ok);
    assert_eq!(fx.runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn manual_mode_never_asks_the_guardian() {
    let fx = fixture();
    let (channel, mut prompts) = attached();
    let options = Options {
        guardian: Some(Scripted::new(vec![deny()])),
        ..Options::default()
    };
    let session = session(&fx, channel, options).await;
    let asked = run(&session, "terminal", json!({"command": "rm -rf build"}));
    answer(&mut prompts, Decision::Deny).await;
    assert_eq!(asked.await.unwrap().status, Status::Denied);
}

/// Approval explain and dry-run: `explain` re-asks with the explanation
/// and nothing runs until a decision.
#[tokio::test]
async fn explain_asks_again_with_the_explanation_and_runs_nothing_until_approved() {
    let fx = fixture();
    let (channel, mut prompts) = attached();
    let session = session(&fx, channel, Options::default()).await;
    let asked = run(&session, "terminal", json!({"command": "rm -rf build/*"}));
    let first = answer(&mut prompts, Decision::Explain).await;
    assert_eq!(first.explanation, None);
    assert_eq!(fx.runs.load(Ordering::SeqCst), 0);
    let second = answer(&mut prompts, Decision::Once).await;
    let explanation = second
        .explanation
        .expect("asked again with the explanation");
    assert!(explanation.contains("program `rm`"), "{explanation}");
    assert!(
        explanation.contains("paths it would touch"),
        "{explanation}"
    );
    assert_eq!(asked.await.unwrap().status, Status::Ok);
    assert_eq!(fx.runs.load(Ordering::SeqCst), 1);
}
