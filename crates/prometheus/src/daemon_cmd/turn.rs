//! A daemon session's way to its clients: its trajectory events as
//! `session.event`, and its approval and clarify prompts as server-to-client
//! requests (spec: workspace-architecture, Server-to-client requests). A
//! prompt no capable client answers resolves unanswered, which the gate
//! treats as a denial.

use std::time::Duration;

use pmt_agent::tools::clarify::{ClarifyAnswer, ClarifyRequest};
use pmt_agent::turn::EventSink;
use pmt_agent::turn::model::BoxFuture;
use pmt_core::human::Surface;
use pmt_gateway::Link;
use pmt_obs::trajectory::writer::Emit;
use pmt_rpc::contracts::{
    ApprovalAsk, ApprovalFinding, ApprovalRequestParams, ClarifyAsk, ClarifyRequestParams,
    Decision, SessionEvent,
};
use pmt_tools::approval::ApprovalRequest;
use serde_json::{Value, json};

/// Forwards every trajectory event of a session to its link.
pub(super) struct ToClients {
    pub(super) link: Link,
}

impl EventSink for ToClients {
    fn emit(&self, emit: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>> {
        let data = json!({
            "type": emit.event.kind(),
            "ts": emit.ts.to_string(),
            "turn_id": emit.turn_id,
            "span_id": emit.span_id,
            "parent_span_id": emit.parent_span_id,
            "payload": emit.event.payload(),
        });
        self.link.emit::<SessionEvent>(&data);
        Box::pin(async { Ok(()) })
    }
}

/// Answers the session's approval prompts through its clients until the
/// session's prompt channel closes.
pub(super) async fn approvals(
    mut prompts: Surface<ApprovalRequest, pmt_tools::approval::Decision>,
    link: Link,
    timeout: Duration,
) {
    while let Some(prompt) = prompts.next().await {
        let params = approval_params(link.session_id(), prompt.request());
        let link = link.clone();
        tokio::spawn(async move {
            let answer = link.ask::<ApprovalAsk>(&params, Some(timeout)).await;
            if let Some(answer) = answer {
                prompt.respond(decision(answer.decision));
            }
            // No answer: dropping the prompt denies it.
        });
    }
}

fn decision(answer: Decision) -> pmt_tools::approval::Decision {
    use pmt_tools::approval::Decision as Gate;
    match answer {
        Decision::Once => Gate::Once,
        Decision::Session => Gate::Session,
        Decision::Always => Gate::Always,
        Decision::Deny => Gate::Deny,
    }
}

fn approval_params(session_id: &str, request: &ApprovalRequest) -> ApprovalRequestParams {
    ApprovalRequestParams {
        session_id: session_id.to_owned(),
        tool: request.tool.clone(),
        call_id: request.call_id.clone(),
        subject: request.subject.clone(),
        findings: request
            .findings
            .iter()
            .map(|f| ApprovalFinding {
                detector: f.detector.clone(),
                reason: f.reason.clone(),
                fragment: f.fragment.clone(),
            })
            .collect(),
        remember: request.remember.clone(),
    }
}

/// Answers the session's clarify questions through its clients.
pub(super) async fn clarify(mut prompts: Surface<ClarifyRequest, ClarifyAnswer>, link: Link) {
    while let Some(prompt) = prompts.next().await {
        let request = prompt.request();
        let params = ClarifyRequestParams {
            session_id: link.session_id().to_owned(),
            questions: request
                .questions
                .iter()
                .map(|q| serde_json::to_value(q).unwrap_or(Value::Null))
                .collect(),
            allow_other: request.allow_other,
        };
        let link = link.clone();
        tokio::spawn(async move {
            let answers = link.ask::<ClarifyAsk>(&params, None).await;
            let parsed =
                answers.and_then(|a| serde_json::from_value(json!({ "answers": a.answers })).ok());
            if let Some(answer) = parsed {
                prompt.respond(answer);
            }
        });
    }
}
