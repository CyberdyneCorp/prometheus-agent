//! What the session sends the TUI: its events become transcript lines, its
//! questions (approvals, clarify, secret and sudo entry) become a card
//! above the composer that the next answer settles, counting down to its
//! deadline, and a request settled elsewhere (answered by another window,
//! timed out, no responder) withdraws its card.

use pmt_rpc::RpcError;
use pmt_rpc::client::ServerCall;
use pmt_rpc::contracts::{
    Admission, COMMAND_NOTICE, Dispatched, Envelope, ModelOptionsList, PROCESS_NOTICE,
    SessionContextUsage, SessionRef, TurnFinished,
};
use pmt_tui::app::Card;
use pmt_tui::transcript::Kind;
use serde_json::{Value, json};

use super::masked;
use super::screen::{Asked, Done, Tui};

/// How to answer an approval card.
const APPROVAL_HINT: &str = "[o]nce  [s]ession  [a]lways  [d]eny  [e]xplain";
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

    /// One event of the connection: the shown session's output reaches
    /// the transcript; the other live sessions only report their turns'
    /// ends.
    pub(super) fn on_event(&mut self, event: &Envelope) {
        if !self.link.applies(event) {
            return;
        }
        let session = event.session_id.clone().unwrap_or_default();
        let shown = session == self.link.session;
        match event.event.as_str() {
            "session.event" if shown => self.on_session_event(&event.data),
            "turn.finished" => {
                if let Ok(finished) = serde_json::from_value::<TurnFinished>(event.data.clone()) {
                    self.on_finished(&session, &finished, shown);
                }
            }
            "request.cancel" => {
                self.withdraw(&event.data, "timed out or nobody could answer it; denied");
            }
            "request.resolved" => self.withdraw(&event.data, "answered in another window"),
            "automation.state" if shown => self.on_automation(&event.data),
            _ => {}
        }
    }

    /// A goal, loop or heartbeat of the shown session changed: its live
    /// row in the status line follows.
    fn on_automation(&mut self, data: &Value) {
        let Ok(state) =
            serde_json::from_value::<pmt_rpc::contracts::AutomationStatePayload>(data.clone())
        else {
            return;
        };
        let rows = &mut self.app.status.automation;
        match state.row {
            Some(row) => {
                rows.insert(state.kind, row);
            }
            None => {
                rows.remove(&state.kind);
            }
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
            Some(PROCESS_NOTICE) => self.notice(format!("[background] {}", text("text"))),
            Some(kind @ ("subagent.spawned" | "subagent.progress" | "subagent.finished")) => {
                if let Some(line) = self.subagents.line(kind, &data["payload"]) {
                    self.notice(line);
                }
            }
            Some(kind) => {
                if let Some(line) =
                    crate::chat::session::progress::context_line(kind, &data["payload"])
                {
                    self.notice(line);
                }
            }
            None => {}
        }
    }

    fn on_finished(&mut self, session: &str, finished: &TurnFinished, shown: bool) {
        if let Some(turns) = self.pending.get_mut(session) {
            turns.remove(&finished.turn_id);
        }
        if !shown {
            self.notice(format!(
                "(session {session} finished a turn in the background)"
            ));
            return;
        }
        if let Some(answer) = &finished.final_response
            && self.app.transcript.last_answer() != Some(answer.as_str())
        {
            self.app.transcript.push(Kind::Assistant, answer.clone());
        }
        if let Some(error) = &finished.error {
            self.error(&error.to_error());
        }
        self.sync_busy();
        self.refresh_status();
    }

    /// A server request: shown when nothing else is asked, else queued.
    pub(super) fn on_request(&mut self, call: ServerCall) {
        match call.method.as_str() {
            "approval.request" | "clarify.request" | masked::SECRET | masked::SUDO => {
                self.asking.push_back(Asked::new(call));
                self.show_question();
            }
            other => {
                let err = RpcError::unknown_method(other);
                self.link.client.respond(&call.id, Err(err));
            }
        }
    }

    /// Puts the first waiting question on the card.
    pub(super) fn show_question(&mut self) {
        self.app.status.approvals = self
            .asking
            .iter()
            .filter(|a| a.method() == "approval.request")
            .count();
        let asked = self.clarify.len();
        let typed = self.masked.chars().count();
        self.app.card = self
            .asking
            .front()
            .map(|a| card(a, asked, typed))
            .or_else(|| self.setup_card())
            .or_else(|| self.prune_card());
    }

    fn withdraw(&mut self, data: &Value, why: &str) {
        let id = data["id"].as_str().unwrap_or_default();
        let before = self.asking.len();
        if self.asking.front().is_some_and(|a| a.call.id == id) {
            self.clarify.clear();
            self.masked.clear();
        }
        self.asking.retain(|a| a.call.id != id);
        if self.asking.len() != before {
            self.notice(format!("(the question {why})"));
            self.show_question();
        }
    }

    /// Answers the question on the card with `line`; `false` when there is
    /// none or `line` is no answer to it (then it is a normal input).
    pub(super) fn answer(&mut self, line: &str) -> bool {
        let Some(asked) = self.asking.front() else {
            return false;
        };
        let call = &asked.call;
        let answer = if call.method == "approval.request" {
            let Some(decision) = crate::chat::remote::turn::decision(line) else {
                return false;
            };
            json!({ "decision": decision })
        } else if masked::is_masked(&call.method) {
            // Answered from the card's own masked input, never a line.
            return false;
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
            Done::Submitted(Ok((session, submitted))) => {
                if let Some(notice) = submitted.notice {
                    self.notice(notice);
                }
                self.admitted(&session, &submitted.turn_id, submitted.admission);
            }
            Done::Submitted(Err(err))
            | Done::Dispatched(Err(err))
            | Done::Switched(Err(err), _) => {
                self.error(&err);
            }
            Done::Dispatched(Ok(dispatched)) => self.dispatched(dispatched),
            Done::Usage(usage) => {
                self.app.status.context_tokens = Some(usage.tokens);
                if usage.context_window.is_some() {
                    self.app.status.context_window = usage.context_window;
                }
                self.app.status.cost_usd = usage.cost_usd;
                self.app.status.billing.clone_from(&usage.billing_source);
                self.app.status.quota = usage.subscription.as_ref().map(|s| s.quota.text.clone());
            }
            Done::Models(models) => {
                if !models.models.is_empty() {
                    self.models = models.models.iter().map(|m| m.id.clone()).collect();
                }
                if let Some(current) = models.current {
                    let window = models.models.iter().find(|m| m.id == current);
                    if let Some(window) = window.and_then(|m| m.context_window) {
                        self.app.status.context_window = Some(window);
                    }
                    self.app.status.model = Some(current);
                }
            }
            Done::Completed(start, completions) => self.completed(start, &completions),
            Done::Switched(Ok((session, attached)), prompt) => {
                self.link.switch(session, &attached).await;
                self.shown_changed();
                self.reload().await;
                let shown = self.link.session.clone();
                self.notice(format!("session {shown}"));
                // `/resume` of a stored session; a new or forked one has none.
                self.memory_summary(&shown).await;
                if let Some(prompt) = prompt {
                    self.submit(prompt, Vec::new());
                }
            }
            Done::Banner(info) => self.show_banner(info),
            Done::Titles(titles) => self.titles_listed(titles),
            Done::Clipboard(grabbed) => self.clipboard_read(grabbed),
            Done::Show(id) => self.shown(&id).await,
            Done::Close(id) => self.closed(&id).await,
            Done::HandedOff(Ok(line)) => self.ending = Some(super::Ending::Detached(line)),
            Done::HandedOff(Err(err)) => self.error(&err),
        }
    }

    fn admitted(&mut self, session: &str, turn: &str, admission: Admission) {
        match admission {
            Admission::Started => {}
            Admission::Queued => self.notice("queued; it runs when the current turn ends"),
            Admission::Steered => {
                self.notice("steering the running turn");
                return;
            }
            // No turn runs (its notice was shown).
            Admission::Handled => return,
        }
        self.pending
            .entry(session.to_owned())
            .or_default()
            .insert(turn.to_owned());
        self.sync_busy();
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

/// The card of a request; for a clarify request, of its question `index`;
/// `typed` is how many characters a secret or sudo card holds.
fn card(asked: &Asked, index: usize, typed: usize) -> Card {
    let within = asked
        .deadline
        .map(|deadline| {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let secs = u64::try_from(left.as_millis().div_ceil(1000)).unwrap_or(u64::MAX);
            format!("   (answer within {secs} s)")
        })
        .unwrap_or_default();
    let p = &asked.call.params;
    match asked.method() {
        "approval.request" => approval_card(p, &within),
        masked::SECRET | masked::SUDO => masked::card(asked.method(), p, typed, &within),
        _ => clarify_card(&p["questions"][index], &within),
    }
}

fn approval_card(p: &Value, within: &str) -> Card {
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
    let mut body = findings.join("\n");
    if let Some(explanation) = p["explanation"].as_str() {
        body.push('\n');
        body.push_str(explanation);
    }
    Card {
        title: format!(
            "approval needed: {} {}",
            p["tool"].as_str().unwrap_or("tool"),
            p["subject"].as_str().unwrap_or_default()
        ),
        body,
        hint: format!("{APPROVAL_HINT}{within}"),
    }
}

fn clarify_card(question: &Value, within: &str) -> Card {
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pmt_rpc::client::ServerCall;
    use serde_json::json;

    use super::{Asked, card};

    fn asked(method: &str, deadline_ms: Option<u64>) -> Asked {
        let mut params = json!({"name": "DEPLOY_TOKEN", "command": "sudo id", "tool": "terminal"});
        if let Some(ms) = deadline_ms {
            params["deadline_ms"] = json!(ms);
        }
        Asked::new(ServerCall {
            id: "srq-000000000001".into(),
            method: method.into(),
            params,
        })
    }

    #[test]
    fn a_card_counts_down_to_its_deadline() {
        let mut waiting = asked("sudo.request", Some(2_500));
        assert!(card(&waiting, 0, 0).hint.ends_with("(answer within 3 s)"));
        waiting.deadline = waiting.deadline.map(|d| d - Duration::from_secs(1));
        assert!(card(&waiting, 0, 0).hint.ends_with("(answer within 2 s)"));
        waiting.deadline = waiting.deadline.map(|d| d - Duration::from_secs(5));
        assert!(card(&waiting, 0, 0).hint.ends_with("(answer within 0 s)"));
        let open = asked("approval.request", None);
        assert!(!card(&open, 0, 0).hint.contains("within"));
    }

    #[test]
    fn a_secret_card_shows_one_bullet_per_typed_character_and_never_the_value() {
        let shown = card(&asked("secret.request", None), 0, 5);
        assert_eq!(shown.title, "secret needed: DEPLOY_TOKEN");
        assert!(shown.body.ends_with("› •••••"), "{}", shown.body);
        let sudo = card(&asked("sudo.request", None), 0, 0);
        assert_eq!(sudo.body, "for: sudo id\n› ");
    }
}
