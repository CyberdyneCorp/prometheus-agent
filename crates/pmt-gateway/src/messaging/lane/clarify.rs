//! Clarify questions in a chat (spec: platform-adapters, Interactive
//! approval cards on every platform; messaging-gateway, Approval requests
//! routed to every surface). Each question renders through the card model:
//! its choices are numbered in the text on every tier and, on `native`
//! adapters, are also signed `clar:` buttons. The requester answers with a
//! button, the choice numbers, or free text; questions are asked one at a
//! time and the request resolves with every answer. A multi-select
//! question gets no buttons here (one press would answer a single choice):
//! its platform controls are 26.2's, until then it is answered by numbers.

use std::fmt::Write as _;
use std::time::Duration;

use jiff::Zoned;
use pmt_store::gateway::outbox::OutboundKind;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::Lane;
use crate::messaging::adapter::{Button, Interactive};
use crate::messaging::cards::{self, Card, CardKind};
use crate::messaging::key::Routing;

/// Told to someone other than the asker who presses a choice.
pub(crate) const NOT_THE_ASKER: &str = "Only the person who asked may answer this.";

/// An open clarify request.
#[derive(Debug, Clone)]
pub(crate) struct Clarify {
    pub(super) id: String,
    requester: String,
    questions: Vec<Value>,
    answers: Vec<Value>,
    /// When the request settles unanswered.
    expires: Option<Instant>,
}

impl Clarify {
    fn theirs(&self, sender: &str) -> bool {
        self.requester.is_empty() || self.requester == sender
    }

    /// Milliseconds left before the request expires.
    fn left_ms(&self) -> Option<u64> {
        self.expires.map(|at| {
            let left = at.saturating_duration_since(Instant::now());
            u64::try_from(left.as_millis()).unwrap_or(u64::MAX)
        })
    }
}

/// The question's choices as text.
fn choices(question: &Value) -> Vec<&str> {
    question["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

impl Lane {
    pub(super) fn clarify_question(&mut self, id: String, params: &Value) {
        let questions = params["questions"].as_array().cloned().unwrap_or_default();
        if questions.is_empty() {
            return;
        }
        let requester = self
            .running
            .as_ref()
            .map_or_else(String::new, |r| r.requester.clone());
        let expires = params["deadline_ms"]
            .as_u64()
            .map(|ms| Instant::now() + Duration::from_millis(ms));
        self.requests.clarify = Some(Clarify {
            id,
            requester,
            questions,
            answers: Vec::new(),
            expires,
        });
        self.ask_next_question();
    }

    /// Sends the next unanswered question as a card.
    fn ask_next_question(&self) {
        let Some(clarify) = &self.requests.clarify else {
            return;
        };
        let index = clarify.answers.len();
        let question = &clarify.questions[index];
        let options = choices(question);
        let title = if clarify.questions.len() > 1 {
            format!("Question {}/{}", index + 1, clarify.questions.len())
        } else {
            "Question".to_owned()
        };
        let card = Card {
            title,
            details: vec![question["question"].as_str().unwrap_or("?").to_owned()],
            deadline_ms: clarify.left_ms(),
            on_expiry: "unanswered, the agent goes on without it",
        };
        let mut text = card.text(&Zoned::now());
        for (n, choice) in options.iter().enumerate() {
            let _ = write!(text, "\n{}. {}", n + 1, cards::visible(choice));
        }
        let multi = question["multi"].as_bool().unwrap_or(false);
        let buttons: Vec<Button> = if self.tier() == Interactive::Native && !multi {
            options
                .iter()
                .enumerate()
                .map(|(n, choice)| Button {
                    label: cards::label(choice),
                    data: self.shared.card_key.data(
                        CardKind::Clarify,
                        &clarify.id,
                        &format!("{index}.{}", n + 1),
                    ),
                })
                .collect()
        } else {
            Vec::new()
        };
        let session = self.session.clone().unwrap_or_default();
        let key = format!("{session}:{}:q{index}", clarify.id);
        let body = json!({"text": text, "buttons": buttons}).to_string();
        self.deliver(key, OutboundKind::Approval, body, None);
    }

    /// The requester's next message answers an open clarify question;
    /// `true` when it did.
    pub(super) fn answer_clarify(&mut self, routing: &Routing, text: &str) -> bool {
        let Some(clarify) = self.requests.clarify.as_ref() else {
            return false;
        };
        if !clarify.theirs(&routing.sender_id) || text.trim_start().starts_with('/') {
            return false;
        }
        let answer = clarify_answer(&clarify.questions[clarify.answers.len()], text);
        self.record_clarify_answer(answer);
        true
    }

    /// A verified `clar:<id>:<question>.<number>` button press.
    pub(super) fn clarify_pressed(&mut self, routing: &Routing, id: &str, choice: &str) {
        let Some(clarify) = self.requests.clarify.as_ref().filter(|c| c.id == id) else {
            tracing::info!("ignored a clarify answer for an unknown or settled request");
            return;
        };
        if !clarify.theirs(&routing.sender_id) {
            self.say(NOT_THE_ASKER);
            return;
        }
        let picked = choice.split_once('.').and_then(|(q, n)| {
            let q: usize = q.parse().ok()?;
            let n: usize = n.parse().ok()?;
            let question = clarify
                .questions
                .get(q)
                .filter(|_| q == clarify.answers.len())?;
            choices(question)
                .get(n.checked_sub(1)?)
                .map(|c| (*c).to_owned())
        });
        let Some(picked) = picked else {
            tracing::info!("ignored a press on an answered clarify question");
            return;
        };
        self.record_clarify_answer(json!({"status": "answered", "choices": [picked]}));
    }

    /// Records the answer to the current question; asks the next one, or
    /// resolves the request with every answer.
    fn record_clarify_answer(&mut self, answer: Value) {
        let Some(clarify) = self.requests.clarify.as_mut() else {
            return;
        };
        clarify.answers.push(answer);
        if clarify.answers.len() < clarify.questions.len() {
            self.ask_next_question();
            return;
        }
        let Some(done) = self.requests.clarify.take() else {
            return;
        };
        let hub = self.shared.server.hub();
        let answer = json!({"answers": done.answers});
        let _ = hub.resolve(
            self.shared.conn,
            &self.shared.peer.client_id,
            &done.id,
            Ok(answer),
        );
    }
}

/// The clarify answer a chat message stands for: choice numbers or text.
fn clarify_answer(question: &Value, line: &str) -> Value {
    let line = line.trim();
    let choices = choices(question);
    let picked: Option<Vec<&str>> = line
        .split(',')
        .map(|n| {
            n.trim()
                .parse::<usize>()
                .ok()
                .and_then(|n| choices.get(n.wrapping_sub(1)).copied())
        })
        .collect();
    match picked {
        Some(picked) if !picked.is_empty() => json!({"status": "answered", "choices": picked}),
        _ => json!({"status": "answered", "text": line}),
    }
}
