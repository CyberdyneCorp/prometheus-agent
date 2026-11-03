//! Approval and clarify prompts in a chat (spec: messaging-gateway,
//! Approval requests routed to every surface). The gateway is one of the
//! session's answering surfaces: an approval becomes a card in the chat
//! (buttons where the platform has them, `/approve` and `/deny` always),
//! the first valid answer from the requester, an admin, or any attached
//! client wins, and the card is then updated with the outcome and who
//! decided; expiry, detachment or dismissal count as a denial. Clarify
//! questions are in `clarify.rs`.

use jiff::Zoned;
use pmt_rpc::contracts::{ApprovalRequestParams, Decision};
use pmt_store::gateway::outbox::OutboundKind;
use serde_json::{Value, json};

use super::Lane;
use super::clarify::Clarify;
use crate::hub::Answered;
use crate::messaging::adapter::{Button, Interactive};
use crate::messaging::auth;
use crate::messaging::cards::{self, Card, CardKind};
use crate::messaging::key::Routing;

/// Told to a bystander who tries to decide.
pub(crate) const NOT_YOURS: &str = "Only the person who asked or an admin may decide this.";
/// `/approve` or `/deny` with nothing pending.
pub(crate) const NOTHING_PENDING: &str = "No pending approval";

#[derive(Debug, Clone)]
struct Approval {
    id: String,
    requester: String,
    card_key: String,
}

/// The prompts open in the conversation, oldest first.
#[derive(Debug, Default)]
pub(crate) struct Open {
    approvals: Vec<Approval>,
    pub(super) clarify: Option<Clarify>,
    /// Secrets already asked for in this chat (told once).
    secrets_told: bool,
}

fn decision_of(word: &str) -> Option<Decision> {
    match word {
        "" | "once" => Some(Decision::Once),
        "session" => Some(Decision::Session),
        "always" => Some(Decision::Always),
        "deny" => Some(Decision::Deny),
        _ => None,
    }
}

fn outcome_text(decision: Decision) -> &'static str {
    match decision {
        Decision::Once => "Approved once",
        Decision::Session => "Approved for this session",
        Decision::Always => "Approved always",
        Decision::Deny => "Denied",
    }
}

/// The approval card's text: title, exact action, working directory, risk
/// reasons, expiry, and how to answer on this tier.
fn card_text(
    params: &ApprovalRequestParams,
    cwd: Option<&str>,
    tier: Interactive,
    id: &str,
    now: &Zoned,
) -> String {
    let mut details = Vec::new();
    if let Some(subject) = &params.subject {
        details.push(format!("Action: {subject}"));
    }
    if let Some(cwd) = cwd {
        details.push(format!("Directory: {cwd}"));
    }
    details.extend(
        params
            .findings
            .iter()
            .map(|finding| format!("Risk: {}", finding.reason)),
    );
    let card = Card {
        title: format!("Approval needed: {}", params.tool),
        details,
        deadline_ms: params.deadline_ms,
        on_expiry: "unanswered, it is denied",
    };
    let mut text = card.text(now);
    match tier {
        Interactive::Native => {
            text.push_str("\nReply /approve, /approve session, /approve always or /deny.");
        }
        other => text.push_str(&cards::instructions(other, id)),
    }
    text
}

impl Lane {
    /// A server request of the session.
    pub(super) fn asked(&mut self, id: String, method: &str, params: &Value) {
        match method {
            "approval.request" => self.approval_card(id, params),
            "clarify.request" => self.clarify_question(id, params),
            "secret.request" | "sudo.request" if !self.requests.secrets_told => {
                self.requests.secrets_told = true;
                self.say(
                    "The agent needs a secret or password, which cannot be entered in a chat; \
                     enter it in a local client (prometheus tui) or it will be skipped.",
                );
            }
            _ => {}
        }
    }

    fn approval_card(&mut self, id: String, params: &Value) {
        let Ok(request) = serde_json::from_value::<ApprovalRequestParams>(params.clone()) else {
            return;
        };
        let requester = self
            .running
            .as_ref()
            .map_or_else(String::new, |r| r.requester.clone());
        let tier = self.tier();
        let buttons = if tier == Interactive::Native {
            cards::CHOICES
                .iter()
                .map(|choice| Button {
                    label: choice[..1].to_uppercase() + &choice[1..],
                    data: self.shared.card_key.data(CardKind::Approval, &id, choice),
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let session = self.session.clone().unwrap_or_default();
        let card_key = format!("{session}:{id}:card");
        let cwd = self.session_cwd(&request.session_id);
        let text = card_text(&request, cwd.as_deref(), tier, &id, &Zoned::now());
        let body = json!({"text": text, "buttons": buttons}).to_string();
        self.deliver(card_key.clone(), OutboundKind::Approval, body, None);
        self.requests.approvals.push(Approval {
            id,
            requester,
            card_key,
        });
    }

    /// Whether `sender` may decide `approval`: its requester or an admin.
    fn may_decide(&self, routing: &Routing, approval: &Approval) -> bool {
        if approval.requester.is_empty() || approval.requester == routing.sender_id {
            return true;
        }
        self.config().is_some_and(|scope| {
            let table = scope.config().gateway.platform(&routing.platform);
            auth::is_admin(&table, routing.chat_type, &routing.sender_id) == Some(true)
        })
    }

    /// The working directory recorded on session `session`.
    fn session_cwd(&self, session: &str) -> Option<String> {
        let store = self.shared.store(&self.profile)?;
        store.session(session.parse().ok()?).ok()?.cwd
    }

    /// How the conversation's adapter renders choices.
    pub(super) fn tier(&self) -> Interactive {
        self.shared
            .adapters
            .descriptor(&self.profile, &self.routing.platform)
            .map_or(Interactive::None, |d| d.interactive)
    }

    /// The pending approval `/approve <id>` or `/deny <id>` names, and the
    /// words after the id; the oldest one when no id is given.
    fn named<'a>(&self, args: &'a str) -> (Option<Approval>, &'a str) {
        let (first, rest) = args.trim().split_once(' ').unwrap_or((args.trim(), ""));
        match self.requests.approvals.iter().find(|a| a.id == first) {
            Some(approval) => (Some(approval.clone()), rest.trim()),
            None => (self.requests.approvals.first().cloned(), args.trim()),
        }
    }

    /// `/approve [<id>] [once|session|always]`: the named or oldest
    /// pending approval.
    pub(super) fn approve(&mut self, routing: &Routing, args: &str) {
        let (approval, rest) = self.named(args);
        let word = rest.split_whitespace().next().unwrap_or_default();
        let Some(decision) = decision_of(&word.to_lowercase()).filter(|d| *d != Decision::Deny)
        else {
            self.say("Usage: /approve [session|always]");
            return;
        };
        let Some(approval) = approval else {
            self.say(NOTHING_PENDING);
            return;
        };
        self.decide(routing, &approval, decision);
    }

    /// `/deny [all | <id>] [reason]`.
    pub(super) fn deny(&mut self, routing: &Routing, args: &str) {
        let all = args.split_whitespace().next() == Some("all");
        let targets: Vec<Approval> = if all {
            self.requests.approvals.clone()
        } else {
            self.named(args).0.into_iter().collect()
        };
        if targets.is_empty() {
            self.say(NOTHING_PENDING);
            return;
        }
        for approval in targets {
            self.decide(routing, &approval, Decision::Deny);
        }
    }

    /// A pressed card button (`appr:` or `clar:<id>:<choice>:<mac>`):
    /// rejected and logged unless its MAC verifies under this boot's key.
    pub(super) fn callback(&mut self, routing: &Routing, data: &str) {
        // A multi-select submission carries its picks after the signed part.
        let (data, picked) = match data.split_once('|') {
            Some((signed, picked)) => (signed, Some(picked)),
            None => (data, None),
        };
        let Some((kind, id, choice)) = self.shared.card_key.verify(data) else {
            tracing::warn!(
                platform = routing.platform,
                sender = routing.sender_id,
                "rejected a card callback whose signature does not verify"
            );
            return;
        };
        match kind {
            CardKind::Clarify => return self.clarify_pressed(routing, id, choice, picked),
            CardKind::Page => return self.page_pressed(routing, id, choice),
            CardKind::Approval => {}
        }
        let Some(decision) = decision_of(choice) else {
            return;
        };
        let found = self.requests.approvals.iter().find(|a| a.id == id).cloned();
        if let Some(approval) = found {
            self.decide(routing, &approval, decision);
        } else {
            tracing::info!("ignored a card answer for an unknown or settled request");
        }
    }

    /// A reply to a numbered card (`1`..`4` or the word, optionally
    /// quoting the prompt id) from someone who may decide; `true` when it
    /// decided.
    pub(super) fn answer_numbered(&mut self, routing: &Routing, text: &str) -> bool {
        if self.requests.approvals.is_empty() || self.tier() != Interactive::Numbered {
            return false;
        }
        let Some((choice, quoted)) = cards::numbered_reply(text) else {
            return false;
        };
        let approval = match quoted {
            Some(id) => self.requests.approvals.iter().find(|a| a.id == id).cloned(),
            None => self.requests.approvals.first().cloned(),
        };
        let Some(approval) = approval.filter(|a| self.may_decide(routing, a)) else {
            return false;
        };
        let Some(decision) = decision_of(choice) else {
            return false;
        };
        self.decide(routing, &approval, decision);
        true
    }

    fn decide(&mut self, routing: &Routing, approval: &Approval, decision: Decision) {
        if !self.may_decide(routing, approval) {
            self.say(NOT_YOURS);
            return;
        }
        let answer = json!({"decision": decision});
        let hub = self.shared.server.hub();
        let by = &self.shared.peer.client_id;
        match hub.resolve(self.shared.conn, by, &approval.id, Ok(answer)) {
            Answered::Taken => {
                let who = format!("{} via {}", routing.sender_id, routing.platform);
                self.close_card(
                    &approval.id,
                    &format!("{} by {who}", outcome_text(decision)),
                );
            }
            Answered::Settled | Answered::Unknown => {
                self.requests.approvals.retain(|a| a.id != approval.id);
                self.say("That approval was already decided.");
            }
        }
    }

    /// Replaces the card of `id` with `outcome` (edited in place where the
    /// platform can, a new message otherwise).
    fn close_card(&mut self, id: &str, outcome: &str) {
        let Some(at) = self.requests.approvals.iter().position(|a| a.id == id) else {
            return;
        };
        let approval = self.requests.approvals.remove(at);
        let message_id = self
            .shared
            .store(&self.profile)
            .and_then(|store| store.outbound_message_id(&approval.card_key).ok().flatten());
        let body = json!({"text": outcome, "buttons": []}).to_string();
        self.deliver(
            format!("{}:outcome", approval.card_key),
            OutboundKind::Approval,
            body,
            message_id,
        );
    }

    /// Another surface answered (the desktop app, the TUI).
    pub(super) fn resolved(&mut self, id: &str, by_client: &str) {
        if by_client == self.shared.peer.client_id {
            return;
        }
        let surface = by_client.split('-').next().unwrap_or(by_client).to_owned();
        if self.requests.approvals.iter().any(|a| a.id == id) {
            let decision = self
                .shared
                .server
                .hub()
                .answer_of(id)
                .and_then(|(value, _)| {
                    serde_json::from_value::<Decision>(value["decision"].clone()).ok()
                });
            let outcome = decision.map_or("Answered", outcome_text);
            self.close_card(id, &format!("{outcome} via {surface}"));
        }
        if self.requests.clarify.as_ref().is_some_and(|c| c.id == id) {
            self.requests.clarify = None;
            self.say(&format!("Answered via {surface}."));
        }
    }

    /// A request settled unanswered: an expired approval is denied.
    pub(super) fn cancelled(&mut self, id: &str, reason: &str) {
        let outcome = if reason == "timeout" {
            "Approval expired; the action was denied."
        } else {
            "Approval withdrawn; the action was denied."
        };
        self.close_card(id, outcome);
        if self.requests.clarify.as_ref().is_some_and(|c| c.id == id) {
            self.requests.clarify = None;
        }
    }
}
