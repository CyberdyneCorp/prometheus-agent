//! Clarify questions in a chat (spec: platform-adapters, Interactive
//! approval cards on every platform; messaging-gateway, Approval requests
//! routed to every surface). Each question renders through the card model:
//! its choices are numbered in the text on every tier and, on `native`
//! adapters, are also signed `clar:` buttons. The requester answers with a
//! button, the choice numbers, or free text; questions are asked one at a
//! time and the request resolves with every answer.
//!
//! A multi-select question (spec: platform-adapters, Clarify multi-select on
//! platforms) says "Multiple selections allowed" and takes numbers
//! separated by commas or spaces (`1, 3`), option texts, or free text as
//! an "Other" answer; `native` adapters also get a multi-select control
//! whose submission carries the picked numbers. A number out of range is
//! answered with the valid range and the question stays open.

use std::fmt::Write as _;
use std::time::Duration;

use jiff::Zoned;
use pmt_store::gateway::outbox::OutboundKind;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::Lane;
use crate::messaging::adapter::{Button, Interactive, Select};
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
        if self.tier() == Interactive::None {
            // Nobody can answer here (a webhook route): the question is
            // answered as unavailable at once instead of waiting it out.
            let unavailable = pmt_core::error::Error::new(
                &pmt_core::error::codes::GATEWAY_NO_ADAPTER,
                "nobody can answer questions in this conversation",
            );
            let _ = self.shared.server.hub().resolve(
                self.shared.conn,
                &self.shared.peer.client_id,
                &id,
                Err(pmt_rpc::RpcError::from_error(&unavailable)),
            );
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
        if multi {
            text.push_str(MULTIPLE);
        }
        let native = self.tier() == Interactive::Native;
        let select = (native && multi && !options.is_empty()).then(|| Select {
            data: self
                .shared
                .card_key
                .data(CardKind::Clarify, &clarify.id, &format!("{index}.m")),
            options: options.iter().map(|c| cards::label(c)).collect(),
        });
        let buttons: Vec<Button> = if native && !multi {
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
        let body = json!({"text": text, "buttons": buttons, "select": select}).to_string();
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
        match clarify_answer(&clarify.questions[clarify.answers.len()], text) {
            Ok(answer) => self.record_clarify_answer(answer),
            Err(count) => self.say(&format!("Valid choices are 1-{count}.")),
        }
        true
    }

    /// A verified `clar:<id>:<question>.<number>` button press, or a
    /// `clar:<id>:<question>.m` multi-select submission of `picked`.
    pub(super) fn clarify_pressed(
        &mut self,
        routing: &Routing,
        id: &str,
        choice: &str,
        picked: Option<&str>,
    ) {
        let Some(clarify) = self.requests.clarify.as_ref().filter(|c| c.id == id) else {
            tracing::info!("ignored a clarify answer for an unknown or settled request");
            return;
        };
        if !clarify.theirs(&routing.sender_id) {
            self.say(NOT_THE_ASKER);
            return;
        }
        let chosen = choice.split_once('.').and_then(|(q, n)| {
            let q: usize = q.parse().ok()?;
            let question = clarify
                .questions
                .get(q)
                .filter(|_| q == clarify.answers.len())?;
            let numbers = match (n, picked) {
                ("m", Some(picked)) => picked
                    .split(',')
                    .map(|p| p.trim().parse().ok())
                    .collect::<Option<Vec<usize>>>()?,
                (n, None) => vec![n.parse().ok()?],
                _ => return None,
            };
            let options = choices(question);
            numbers
                .iter()
                .map(|n| options.get(n.checked_sub(1)?).map(|c| (*c).to_owned()))
                .collect::<Option<Vec<String>>>()
                .filter(|c| !c.is_empty())
        });
        let Some(chosen) = chosen else {
            tracing::info!("ignored a press on an answered clarify question");
            return;
        };
        self.record_clarify_answer(json!({"status": "answered", "choices": chosen}));
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

/// Appended to a multi-select question.
const MULTIPLE: &str = "\nMultiple selections allowed";

/// The clarify answer a chat message stands for: choice numbers (commas
/// or spaces between them on a multi-select question), option texts, or
/// free text; `Err(count)` for a number outside `1..=count`.
fn clarify_answer(question: &Value, line: &str) -> Result<Value, usize> {
    let line = line.trim();
    let choices = choices(question);
    let multi = question["multi"].as_bool().unwrap_or(false);
    let separators: &[char] = if multi { &[',', ' '] } else { &[','] };
    let numbers: Option<Vec<usize>> = line
        .split(separators)
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.trim().parse().ok())
        .collect();
    if let Some(numbers) = numbers.filter(|n| !n.is_empty() && !choices.is_empty()) {
        if numbers.iter().any(|n| *n == 0 || *n > choices.len()) {
            return Err(choices.len());
        }
        let picked: Vec<&str> = numbers.iter().map(|n| choices[n - 1]).collect();
        return Ok(json!({"status": "answered", "choices": picked}));
    }
    let named = |text: &str| {
        choices
            .iter()
            .find(|c| c.eq_ignore_ascii_case(text.trim()))
            .copied()
    };
    let parts: Vec<&str> = if multi {
        line.split(',').collect()
    } else {
        vec![line]
    };
    let picked: Option<Vec<&str>> = parts.iter().map(|p| named(p)).collect();
    Ok(match picked {
        Some(picked) if !picked.is_empty() => json!({"status": "answered", "choices": picked}),
        _ => json!({"status": "answered", "text": line}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(multi: bool) -> Value {
        json!({"question": "Which?", "choices": ["red", "green", "blue", "gold"], "multi": multi})
    }

    #[test]
    fn numbered_multi_answers_pick_each_option() {
        assert_eq!(
            clarify_answer(&question(true), "1, 3").unwrap(),
            json!({"status": "answered", "choices": ["red", "blue"]})
        );
        assert_eq!(
            clarify_answer(&question(true), "2 4").unwrap(),
            json!({"status": "answered", "choices": ["green", "gold"]})
        );
        assert_eq!(
            clarify_answer(&question(true), "Red, gold").unwrap(),
            json!({"status": "answered", "choices": ["red", "gold"]})
        );
        assert_eq!(
            clarify_answer(&question(true), "something else").unwrap(),
            json!({"status": "answered", "text": "something else"})
        );
    }

    #[test]
    fn out_of_range_numbers_name_the_valid_range() {
        assert_eq!(clarify_answer(&question(true), "7"), Err(4));
        assert_eq!(clarify_answer(&question(false), "0"), Err(4));
        assert_eq!(
            clarify_answer(&question(false), "blue").unwrap(),
            json!({"status": "answered", "choices": ["blue"]})
        );
    }
}
