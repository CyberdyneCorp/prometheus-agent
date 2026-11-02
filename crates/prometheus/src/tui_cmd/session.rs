//! What the session sends the TUI: its events become transcript lines, its
//! questions (approvals, clarify) become a card above the composer that
//! the next answer settles, and a request settled elsewhere (answered by
//! another window, timed out, no responder) withdraws its card.

use pmt_rpc::RpcError;
use pmt_rpc::client::ServerCall;
use pmt_rpc::contracts::{
    Admission, COMMAND_NOTICE, Dispatched, Envelope, ModelOptionsList, SessionContextUsage,
    SessionRef, TurnFinished,
};
use pmt_tui::app::Card;
use pmt_tui::transcript::Kind;
use serde_json::{Value, json};

use super::screen::{Done, Tui};

/// How to answer an approval card.
const APPROVAL_HINT: &str = "[o]nce  [s]ession  [a]lways  [d]eny";
/// How to answer a question card.
const CLARIFY_HINT: &str = "type a number, your own answer, or nothing to skip; Enter sends";

impl Tui<'_> {
    /// Reloads the transcript and the status (on open, after a switch, or
    /// when a re-attachment could not replay what was missed).
    pub(super) async fn reload(&mut self) {
        self.app.transcript.clear();
        self.app.scroll = 0;
        match self.link.transcript().await {
            Ok(transcript) => {
                for message in transcript.messages {
                    let kind = match message.role.as_str() {
                        "user" => Kind::User,
                        "assistant" => Kind::Assistant,
                        _ => continue,
                    };
                    if let Some(text) = message.text.filter(|t| !t.is_empty()) {
                        self.app.transcript.push(kind, text);
                    }
                }
            }
            Err(err) => self.error(&err),
        }
        self.refresh_status();
    }

    /// Asks for the context use, cost and model in the background.
    pub(super) fn refresh_status(&self) {
        let params = SessionRef {
            session_id: self.link.session.clone(),
        };
        let client = self.link.client.clone();
        let usage = params.clone();
        self.background(async move {
            match client.call_typed::<SessionContextUsage>(&usage).await {
                Ok(usage) => Done::Usage(usage),
                Err(_) => Done::Models(pmt_rpc::contracts::ModelOptions::default()),
            }
        });
        let client = self.link.client.clone();
        self.background(async move {
            Done::Models(
                client
                    .call_typed::<ModelOptionsList>(&params)
                    .await
                    .unwrap_or_default(),
            )
        });
    }

    /// One event of the connection.
    pub(super) fn on_event(&mut self, event: &Envelope) {
        if !self.link.applies(event) {
            return;
        }
        match event.event.as_str() {
            "session.event" => self.on_session_event(&event.data),
            "turn.finished" => {
                if let Ok(finished) = serde_json::from_value::<TurnFinished>(event.data.clone()) {
                    self.on_finished(&finished);
                }
            }
            "request.cancel" => {
                self.withdraw(&event.data, "timed out or nobody could answer it; denied");
            }
            "request.resolved" => self.withdraw(&event.data, "answered in another window"),
            _ => {}
        }
    }

    fn on_session_event(&mut self, data: &Value) {
        let text = |key: &str| data["payload"][key].as_str().unwrap_or_default().to_owned();
        match data["type"].as_str() {
            Some("user.message") => self.app.transcript.push(Kind::User, text("text")),
            Some("assistant.message") if !text("text").is_empty() => {
                self.app.transcript.push(Kind::Assistant, text("text"));
            }
            Some("tool.call") => self.app.transcript.push(Kind::Tool, text("tool")),
            Some(COMMAND_NOTICE) => self.notice(text("text")),
            _ => {}
        }
    }

    fn on_finished(&mut self, finished: &TurnFinished) {
        if let Some(answer) = &finished.final_response
            && self.app.transcript.last_answer() != Some(answer.as_str())
        {
            self.app.transcript.push(Kind::Assistant, answer.clone());
        }
        if let Some(error) = &finished.error {
            self.error(&error.to_error());
        }
        self.pending.remove(&finished.turn_id);
        self.app.status.busy = !self.pending.is_empty();
        self.refresh_status();
    }

    /// A server request: shown when nothing else is asked, else queued.
    pub(super) fn on_request(&mut self, call: ServerCall) {
        match call.method.as_str() {
            "approval.request" | "clarify.request" => {
                self.asking.push_back(call);
                self.show_question();
            }
            _ => {
                let err = RpcError::from_error(&pmt_core::Error::new(
                    &pmt_core::error::codes::CHAT_APPROVAL_REQUIRED,
                    "the TUI does not answer this request",
                ));
                self.link.client.respond(&call.id, Err(err));
            }
        }
    }

    /// Puts the first waiting question on the card.
    pub(super) fn show_question(&mut self) {
        self.app.status.approvals = self
            .asking
            .iter()
            .filter(|c| c.method == "approval.request")
            .count();
        let asked = self.clarify.len();
        self.app.card = self.asking.front().map(|call| card(call, asked));
    }

    fn withdraw(&mut self, data: &Value, why: &str) {
        let id = data["id"].as_str().unwrap_or_default();
        let before = self.asking.len();
        if self.asking.front().is_some_and(|c| c.id == id) {
            self.clarify.clear();
        }
        self.asking.retain(|c| c.id != id);
        if self.asking.len() != before {
            self.notice(format!("(the question {why})"));
            self.show_question();
        }
    }

    /// Answers the question on the card with `line`; `false` when there is
    /// none or `line` is no answer to it (then it is a normal input).
    pub(super) fn answer(&mut self, line: &str) -> bool {
        let Some(call) = self.asking.front() else {
            return false;
        };
        let answer = if call.method == "approval.request" {
            let Some(decision) = crate::chat::remote::turn::decision(line) else {
                return false;
            };
            json!({ "decision": decision })
        } else {
            let questions = call.params["questions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let question = questions
                .get(self.clarify.len())
                .cloned()
                .unwrap_or_default();
            self.clarify
                .push(crate::chat::remote::turn::clarify_answer(&question, line));
            if self.clarify.len() < questions.len() {
                self.show_question();
                return true;
            }
            json!({ "answers": std::mem::take(&mut self.clarify) })
        };
        let id = call.id.clone();
        self.link.client.respond(&id, Ok(answer));
        self.asking.pop_front();
        self.show_question();
        true
    }

    /// A background request finished.
    pub(super) async fn on_done(&mut self, done: Done) {
        match done {
            Done::Submitted(Ok(submitted)) => {
                self.admitted(&submitted.turn_id, submitted.admission);
            }
            Done::Submitted(Err(err)) | Done::Dispatched(Err(err)) | Done::Switched(Err(err)) => {
                self.error(&err);
            }
            Done::Dispatched(Ok(dispatched)) => self.dispatched(dispatched),
            Done::Usage(usage) => {
                self.app.status.context_tokens = Some(usage.tokens);
                if usage.context_window.is_some() {
                    self.app.status.context_window = usage.context_window;
                }
                self.app.status.cost_usd = usage.cost_usd;
            }
            Done::Models(models) => {
                if let Some(current) = models.current {
                    let window = models.models.iter().find(|m| m.id == current);
                    if let Some(window) = window.and_then(|m| m.context_window) {
                        self.app.status.context_window = Some(window);
                    }
                    self.app.status.model = Some(current);
                }
            }
            Done::Completed(start, completions) => self.completed(start, &completions),
            Done::Switched(Ok((session, attached))) => {
                self.link.switch(session, &attached).await;
                self.pending.clear();
                self.app.status.busy = false;
                self.reload().await;
                let shown = self.link.session.clone();
                self.notice(format!("session {shown}"));
            }
        }
    }

    fn admitted(&mut self, turn: &str, admission: Admission) {
        match admission {
            Admission::Started => {}
            Admission::Queued => self.notice("queued; it runs when the current turn ends"),
            Admission::Steered => {
                self.notice("steering the running turn");
                return;
            }
        }
        self.pending.insert(turn.to_owned());
        self.app.status.busy = true;
    }

    fn dispatched(&mut self, dispatched: Dispatched) {
        match dispatched {
            Dispatched::Done { notice } if notice.is_empty() => {}
            Dispatched::Done { notice } => self.notice(notice),
            Dispatched::Send { message, images } => self.submit(message, images),
            Dispatched::Refused { reason } => {
                self.app.transcript.push(Kind::Error, reason);
            }
        }
    }
}

/// The card of a request; for a clarify request, of its question `index`.
fn card(call: &ServerCall, index: usize) -> Card {
    let p = &call.params;
    let within = p["deadline_ms"]
        .as_u64()
        .map(|ms| format!("   (answer within {} s)", ms.div_ceil(1000)))
        .unwrap_or_default();
    if call.method == "approval.request" {
        let findings: Vec<String> = p["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                format!(
                    "{}: {} ({})",
                    f["detector"].as_str().unwrap_or_default(),
                    f["reason"].as_str().unwrap_or_default(),
                    f["fragment"].as_str().unwrap_or_default()
                )
            })
            .collect();
        return Card {
            title: format!(
                "approval needed: {} {}",
                p["tool"].as_str().unwrap_or("tool"),
                p["subject"].as_str().unwrap_or_default()
            ),
            body: findings.join("\n"),
            hint: format!("{APPROVAL_HINT}{within}"),
        };
    }
    let question = &p["questions"][index];
    let choices: Vec<String> = question["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(n, c)| format!("{}. {}", n + 1, c.as_str().unwrap_or_default()))
        .collect();
    Card {
        title: question["question"].as_str().unwrap_or_default().to_owned(),
        body: choices.join("\n"),
        hint: format!("{CLARIFY_HINT}{within}"),
    }
}
