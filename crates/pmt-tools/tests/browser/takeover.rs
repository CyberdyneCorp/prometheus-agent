//! The bot screen takeover lease fences the browser tools (spec:
//! execution-environments, Bot screen takeover lease).

use std::sync::Arc;

use pmt_config::schema::Config;
use pmt_config::screen_lease::{ScreenLease, now};
use pmt_core::cancel::CancelToken;
use pmt_tools::browser::fence::fenced;
use pmt_tools::descriptor::{
    CallContext, Executor, HandlerFuture, Owner, RegisteredTool, ToolDescriptor, ToolHandler,
};
use pmt_tools::environments::{Environments, SessionEnv};
use pmt_tools::result::{Status, ToolResult};
use serde_json::json;

use crate::dispatcher::{call, dispatcher};
use crate::support::Fixture;

const SESSION: &str = "ses_b";

/// Scenario: Agent during takeover. While a person holds the screen, the
/// agent's browser call fails with `human_has_control` before any browser
/// starts; once handed back, the fence lets calls through again.
#[tokio::test]
async fn browser_tools_wait_while_a_person_holds_the_screen() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let environments = Arc::new(Environments::default());
    let _attached = environments.attach(
        SESSION,
        SessionEnv {
            browser: Some(Arc::new(fixture.exec(&manager, false))),
            ..SessionEnv::default()
        },
    );
    let lease = ScreenLease::of(&fixture.profile);
    lease.take("desktop", now()).unwrap();
    let tools = dispatcher(
        &environments,
        SESSION,
        &Config::default(),
        &["process.spawn:*", "net.connect:*"],
        &fixture.profile,
    )
    .await;
    let refused = call(
        &tools,
        "browser_exec",
        json!({"code": "print(page_info())"}),
    )
    .await;
    assert_eq!(refused.status, Status::Error, "{}", refused.text_content());
    assert!(
        refused.text_content().contains("human_has_control"),
        "{}",
        refused.text_content()
    );
    assert!(
        crate::support::records(&fixture.browser_cache()).is_empty(),
        "no browser started"
    );
    lease.hand_back("desktop", false, now()).unwrap().unwrap();
    assert!(lease.guard(now()).is_ok());
    manager.shutdown().await;
}

/// A handler that runs while the person takes the screen over.
struct TakenMidCall(ScreenLease);

impl ToolHandler for TakenMidCall {
    fn call<'a>(&'a self, _: &'a CallContext, _: serde_json::Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            self.0.take("desktop", now()).unwrap();
            Ok(ToolResult::ok("clicked"))
        })
    }
}

/// A call that started before the takeover has its result voided by the
/// lease epoch.
#[tokio::test]
async fn a_result_from_before_the_takeover_is_voided() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let environments = Arc::new(Environments::default());
    let _attached = environments.attach(
        SESSION,
        SessionEnv {
            browser: Some(Arc::new(fixture.exec(&manager, false))),
            ..SessionEnv::default()
        },
    );
    let lease = ScreenLease::of(&fixture.profile);
    let tool = fenced(
        RegisteredTool::new(
            ToolDescriptor::new(
                "browser_probe",
                "browser",
                "test",
                json!({"type": "object"}),
            ),
            Owner::Builtin,
            Arc::new(TakenMidCall(lease.clone())),
        ),
        &environments,
    );
    let Executor::Handler(handler) = &tool.executor else {
        unreachable!("a handler tool");
    };
    let context = CallContext {
        call_id: "call_1".to_owned(),
        session_id: SESSION.to_owned(),
        cancel: CancelToken::new(),
    };
    let failure = handler.call(&context, json!({})).await.unwrap_err();
    assert_eq!(failure.kind, "human_has_control");
    assert!(failure.message.contains("void"), "{}", failure.message);
    manager.shutdown().await;
}
