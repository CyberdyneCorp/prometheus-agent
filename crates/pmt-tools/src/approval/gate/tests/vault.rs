//! Card fills need a human approval every time (spec: security-boundaries,
//! Vault codes, cards and addresses): through the real dispatcher, a
//! webhook session's card fill is refused before the tool runs, and an
//! interactive one asks even under yolo.

use super::*;

fn fill(handle: &str) -> Value {
    json!({"action": "fill", "handle": handle})
}

/// Scenario: Card fill from a webhook session.
#[tokio::test]
async fn a_card_fill_from_a_webhook_session_is_refused_before_anything_runs() {
    let fx = fixture();
    let options = Options {
        context: Context::Unattended,
        approvals: ApprovalsConfig {
            unattended_mode: UnattendedApproval::Approve,
            ..ApprovalsConfig::default()
        },
        ..Options::default()
    };
    let session = session(&fx, Channel::unattended(), options).await;
    let result = run(&session, "browser_vault", fill("card_00aa11bb22cc"))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Denied, "{}", result.text_content());
    assert!(
        result.text_content().contains("needs a human approval"),
        "{}",
        result.text_content()
    );
    // An address fill is not gated (it fails here only because the test
    // session has no browser).
    let address = run(&session, "browser_vault", fill("address_00aa11bb22cc"))
        .await
        .unwrap();
    assert_ne!(address.status, Status::Denied, "{}", address.text_content());
}

#[tokio::test]
async fn an_interactive_card_fill_asks_every_time_even_under_yolo() {
    let fx = fixture();
    let (channel, mut prompts) = attached();
    let options = Options {
        yolo: true,
        approvals: ApprovalsConfig {
            mode: ApprovalMode::Off,
            ..ApprovalsConfig::default()
        },
        ..Options::default()
    };
    let session = session(&fx, channel, options).await;
    for _ in 0..2 {
        let call = run(&session, "browser_vault", fill("card_00aa11bb22cc"));
        let asked = answer(&mut prompts, Decision::Session).await;
        assert_eq!(asked.findings[0].detector, "plugin_rule:vault_card_fill");
        assert_eq!(asked.remember, None, "never remembered");
        call.await.unwrap();
    }
}
