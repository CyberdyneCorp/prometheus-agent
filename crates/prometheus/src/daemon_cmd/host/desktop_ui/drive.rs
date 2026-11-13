//! `drive_preview` (spec: desktop-app, Driving the preview page): the
//! window acts on the page; typing into a password field waits for the
//! user's approval and a navigation the action caused must pass the
//! preview's URL safety policy.
//!
//! The window answers a `type` aimed at a password field with
//! `needs_approval` and types nothing; the daemon then raises an
//! `approval.request` and repeats the call with `approved` only when the
//! user allowed it.

use pmt_rpc::contracts::{ApprovalAsk, ApprovalFinding, ApprovalRequestParams, Decision};
use pmt_tools::ToolResult;
use serde_json::{Value, json};

use super::{UiPort, answer, check, fail, window};

/// Runs one `drive_preview` call.
pub(super) async fn call(port: &UiPort, mut args: Value) -> ToolResult {
    if let Err(refused) = check::refusal("drive_preview", &args) {
        return fail(refused);
    }
    // Only the daemon grants a password field: the model cannot.
    if let Some(fields) = args.as_object_mut() {
        fields.remove("approved");
    }
    let timeout = check::deadline("drive_preview", &args);
    let mut value = match window::ask(port, "drive_preview", args.clone(), timeout).await {
        Ok(value) => value,
        Err(unanswered) => return unanswered.result("drive_preview", timeout),
    };
    if value.get("needs_approval").and_then(Value::as_bool) == Some(true) {
        let field = value
            .get("field")
            .and_then(Value::as_str)
            .unwrap_or("a password field")
            .to_owned();
        if !approved(port, &field).await {
            return fail(format!("the user did not allow typing into {field}"));
        }
        args["approved"] = json!(true);
        value = match window::ask(port, "drive_preview", args, timeout).await {
            Ok(value) => value,
            Err(unanswered) => return unanswered.result("drive_preview", timeout),
        };
    }
    if let Some(url) = value.get("url").and_then(Value::as_str).map(str::to_owned)
        && url.starts_with("http")
        && let Err(refused) = window::preview_url(port, &url).await
    {
        // Leave the refused page before reporting it.
        let back = json!({"action": "back"});
        let _ = window::ask(port, "drive_preview", back, timeout).await;
        return fail(format!(
            "the page navigated to {url}, which the preview refuses: {refused}"
        ));
    }
    answer(&value)
}

/// Asks the user whether the agent may type into `field`.
async fn approved(port: &UiPort, field: &str) -> bool {
    let Some(link) = port.link() else {
        return false;
    };
    let params = ApprovalRequestParams {
        session_id: port.session.to_string(),
        tool: "drive_preview".to_owned(),
        call_id: "drive_preview".to_owned(),
        subject: Some(format!(
            "type into the password field {field} of the preview page"
        )),
        findings: vec![ApprovalFinding {
            detector: "password-field".to_owned(),
            reason: "the agent wants to type into a password field".to_owned(),
            fragment: field.to_owned(),
        }],
        remember: None,
        deadline_ms: None,
        explanation: None,
    };
    let answer = link.ask::<ApprovalAsk>(&params, None).await;
    matches!(
        answer.map(|a| a.decision),
        Some(Decision::Once | Decision::Session | Decision::Always)
    )
}
