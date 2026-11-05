//! `browser_cdp` and `browser_dialog` against a browser the user runs
//! (spec: execution-environments, `browser_cdp` and `browser_dialog`
//! tools).

use std::time::Duration;

use pmt_tools::browser::cdp_tools::CdpCall;
use pmt_tools::browser::{BrowserSession, Origin};
use serde_json::{Value, json};

use crate::support::{self, External, Fixture, site};

fn call(method: &str, params: Value, target: Option<&str>) -> CdpCall {
    CdpCall {
        method: method.to_owned(),
        params,
        target_id: target.map(str::to_owned),
        ..CdpCall::default()
    }
}

async fn evaluate(browser: &BrowserSession, target: &str, expression: &str) -> Value {
    let reply = browser
        .cdp_call(&call(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true}),
            Some(target),
        ))
        .await
        .unwrap();
    reply["result"]["value"].clone()
}

/// A connected session over a browser this test starts.
async fn connected(
    fixture: &Fixture,
) -> (
    External,
    std::sync::Arc<pmt_tools::browser::BrowserManager>,
    std::sync::Arc<BrowserSession>,
) {
    let executable = support::browsers()[0].clone();
    let external = External::start(&executable, &fixture.home.join("external"));
    let manager = fixture.manager();
    manager.set_connect("ses_b", Some(external.http()));
    let browser = manager.session("ses_b/default").await.unwrap();
    assert_eq!(browser.origin(), &Origin::Connected(external.http()));
    (external, manager, browser)
}

/// Scenario: Dialog answered.
#[tokio::test]
async fn a_pending_confirm_is_accepted_and_the_page_resumes() {
    let fixture = Fixture::new();
    let page = site().await;
    let (external, manager, browser) = connected(&fixture).await;
    let created = browser
        .cdp_call(&call(
            "Target.createTarget",
            json!({"url": page.uri()}),
            None,
        ))
        .await
        .unwrap();
    let target = created["targetId"].as_str().unwrap().to_owned();
    // In front, so a busy host does not throttle the page's timer.
    browser
        .cdp_call(&call("Page.bringToFront", json!({}), Some(&target)))
        .await
        .unwrap();
    evaluate(
        &browser,
        &target,
        "setTimeout(() => { window.answer = confirm('Proceed?'); }, 0); true",
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while browser.dialogs().pending().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no dialog was seen");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(browser.dialogs().pending()[0].message, "Proceed?");
    browser.answer_dialog(None, true, None).await.unwrap();
    assert_eq!(
        evaluate(&browser, &target, "window.answer").await,
        json!(true)
    );
    assert_eq!(
        browser.answer_dialog(None, true, None).await,
        Err("no pending dialog".to_owned())
    );
    manager.shutdown().await;
    external.stop();
}

/// Scenario: Closing the browser.
#[tokio::test]
async fn closing_or_crashing_the_browser_and_foreign_targets_are_refused() {
    let fixture = Fixture::new();
    let (mut external, manager, browser) = connected(&fixture).await;
    for method in ["Browser.close", "Browser.crash"] {
        let refused = browser.cdp_call(&call(method, json!({}), None)).await;
        assert_eq!(refused, Err(format!("{method} is not allowed")));
    }
    let targets = browser
        .cdp_call(&call("Target.getTargets", json!({}), None))
        .await
        .unwrap();
    let foreign = targets["targetInfos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["type"] == "page")
        .and_then(|t| t["targetId"].as_str())
        .unwrap()
        .to_owned();
    let refused = browser
        .cdp_call(&call(
            "Target.closeTarget",
            json!({"targetId": foreign}),
            None,
        ))
        .await
        .unwrap_err();
    assert!(refused.contains("this session opened"), "{refused}");
    // A page-level command names its target.
    let missing = browser
        .cdp_call(&call("Runtime.evaluate", json!({"expression": "1"}), None))
        .await
        .unwrap_err();
    assert!(missing.contains("target_id"), "{missing}");
    // A URL the browser URL safety refuses never reaches the browser.
    let metadata = browser
        .cdp_call(&call(
            "Target.createTarget",
            json!({"url": "http://169.254.169.254/latest/meta-data"}),
            None,
        ))
        .await
        .unwrap_err();
    assert!(metadata.contains("metadata"), "{metadata}");
    // A target this session opened may be closed.
    let own = browser
        .cdp_call(&call(
            "Target.createTarget",
            json!({"url": "about:blank"}),
            None,
        ))
        .await
        .unwrap();
    let own = own["targetId"].as_str().unwrap();
    browser
        .cdp_call(&call("Target.closeTarget", json!({"targetId": own}), None))
        .await
        .unwrap();
    assert!(external.running(), "the browser was not closed");
    let version = browser
        .cdp_call(&call("Browser.getVersion", json!({}), None))
        .await
        .unwrap();
    assert!(version["product"].as_str().is_some());
    manager.shutdown().await;
    external.stop();
}

#[tokio::test]
async fn a_frame_id_of_a_same_process_frame_goes_through_its_page() {
    let fixture = Fixture::new();
    let (external, manager, browser) = connected(&fixture).await;
    let created = browser
        .cdp_call(&call(
            "Target.createTarget",
            json!({"url": "data:text/html,<iframe srcdoc='<p>inner</p>'></iframe>"}),
            None,
        ))
        .await
        .unwrap();
    let target = created["targetId"].as_str().unwrap().to_owned();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let tree = browser
        .cdp_call(&call("Page.getFrameTree", json!({}), Some(&target)))
        .await
        .unwrap();
    let frame = tree["frameTree"]["childFrames"][0]["frame"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let frame_call = CdpCall {
        frame_id: Some(frame.clone()),
        target_id: Some(target.clone()),
        ..call("Page.getFrameTree", json!({}), None)
    };
    assert!(browser.cdp_call(&frame_call).await.is_ok());
    let without_page = CdpCall {
        frame_id: Some(frame),
        ..call("Page.getFrameTree", json!({}), None)
    };
    let refused = browser.cdp_call(&without_page).await.unwrap_err();
    assert!(refused.contains("not an out-of-process frame"), "{refused}");
    manager.shutdown().await;
    external.stop();
}
