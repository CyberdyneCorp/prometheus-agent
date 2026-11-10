//! A daemon session's way to its clients: its trajectory events as
//! `session.event`, and its approval and clarify prompts as server-to-client
//! requests (spec: workspace-architecture, Server-to-client requests), and
//! a skill's missing secret as `secret.request` and a sudo password as
//! `sudo.request`. A prompt no capable
//! client answers resolves unanswered, which the gate treats as a denial
//! (a secret: reported missing to the model).

use std::time::Duration;

use pmt_agent::tools::clarify::{ClarifyAnswer, ClarifyRequest};
use pmt_agent::turn::EventSink;
use pmt_agent::turn::model::BoxFuture;
use pmt_core::human::Surface;
use pmt_core::secret::SecretString;
use pmt_gateway::Link;
use pmt_obs::trajectory::writer::Emit;
use pmt_rpc::contracts::{
    ASSISTANT_DELTA, ApprovalAsk, ApprovalFinding, ApprovalRequestParams, ClarifyAsk,
    ClarifyRequestParams, Decision, SecretAsk, SecretRequestParams, SessionEvent, SudoAsk,
    SudoRequestParams,
};
use pmt_skills::secrets::SecretPrompt;
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

/// Streams a turn's answer text: to the attached clients as
/// `assistant.delta` session events (a voice client speaks the reply while
/// it generates; spec: voice-mode, Sentence-streamed spoken replies) and
/// to the link's own receiver (a messaging conversation that streams;
/// spec: messaging-gateway, Streaming to platforms).
pub(super) struct DeltasToLink {
    pub(super) link: Link,
    /// The turn's id as `prompt.submit` answered it.
    pub(super) turn_id: String,
}

impl pmt_agent::turn::DeltaSink for DeltasToLink {
    fn text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let data = json!({
            "type": ASSISTANT_DELTA,
            "ts": jiff::Timestamp::now().to_string(),
            "turn_id": self.turn_id,
            "span_id": null,
            "parent_span_id": null,
            "payload": {"text": text},
        });
        self.link.emit::<SessionEvent>(&data);
        self.link.delta(text);
    }
}

/// Asks the session's clients for a skill's missing secret. The skills
/// engine calls it on the blocking pool and stores the value itself in
/// the profile's secrets store; it never reaches the transcript.
pub(super) struct SecretsToClients {
    pub(super) link: Link,
    /// The daemon's runtime (the caller is a blocking-pool thread).
    pub(super) runtime: tokio::runtime::Handle,
}

impl SecretPrompt for SecretsToClients {
    fn ask(&self, skill: &str, name: &str, prompt: Option<&str>) -> Option<SecretString> {
        let reason = prompt.map_or_else(
            || format!("skill {skill} needs it"),
            |p| format!("skill {skill}: {p}"),
        );
        let params = SecretRequestParams {
            session_id: self.link.session_id().to_owned(),
            name: name.to_owned(),
            reason: Some(reason),
            deadline_ms: None,
        };
        let answer = self
            .runtime
            .block_on(self.link.ask::<SecretAsk>(&params, None))?;
        answer
            .value
            .filter(|v| !v.is_empty())
            .map(SecretString::new)
    }
}

/// Asks a daemon session's clients for the sudo password
/// (`sudo.request`; spec: execution-environments, Sudo password handling).
pub(super) struct SudoToClients {
    pub(super) link: Link,
    /// The daemon's runtime (the caller is a blocking-pool thread).
    pub(super) runtime: tokio::runtime::Handle,
}

impl pmt_tools::terminal::sudo::SudoPrompt for SudoToClients {
    fn ask(&self, command: &str) -> Option<SecretString> {
        let params = SudoRequestParams {
            session_id: self.link.session_id().to_owned(),
            command: command.to_owned(),
            deadline_ms: None,
        };
        let answer = self
            .runtime
            .block_on(self.link.ask::<SudoAsk>(&params, None))?;
        answer
            .password
            .filter(|p| !p.is_empty())
            .map(SecretString::new)
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
        pmt_core::spawn::spawn(async move {
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
        Decision::Explain => Gate::Explain,
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
        deadline_ms: None,
        explanation: request.explanation.clone(),
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
            deadline_ms: None,
        };
        let link = link.clone();
        pmt_core::spawn::spawn(async move {
            let answers = link.ask::<ClarifyAsk>(&params, None).await;
            let parsed =
                answers.and_then(|a| serde_json::from_value(json!({ "answers": a.answers })).ok());
            if let Some(answer) = parsed {
                prompt.respond(answer);
            }
        });
    }
}
