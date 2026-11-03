//! What one linked device remembers between events (spec: platform-
//! adapters, `WhatsApp` pairing and modes; `WhatsApp` interactive and rich
//! features): which events count (`bot` mode answers others, `self_chat`
//! only the user's chat with themselves; groups by `group_policy`), the
//! adapter's last 1000 sends (their echoes are not inbound and a quote of
//! one is resolved to its text), the polls it opened for clarify
//! questions (a vote is the answer) and the numbered choices of an
//! approval card (a number answers it; approvals never become polls).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use pmt_config::platforms::{GroupPolicy, WhatsappMode};
use pmt_gateway::messaging::{Button, ChatType, Inbound, Select};
use serde_json::Value;

/// Own sends remembered.
const OWN_SENDS: usize = 1000;

/// What a vote on a poll answers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PollAnswer {
    /// One signed button per option (a single-choice question).
    Buttons(Vec<String>),
    /// A multi-select control's signed data.
    Select(String),
}

/// A poll the adapter opened.
#[derive(Debug, Clone)]
struct Poll {
    options: Vec<String>,
    answer: PollAnswer,
}

/// The device's memory.
#[derive(Debug, Default)]
pub(crate) struct Session {
    pub(crate) mode: WhatsappMode,
    pub(crate) group_policy: GroupPolicy,
    /// Groups admitted under the `allowlist` and `pairing` policies.
    pub(crate) allowed_groups: Vec<String>,
    own: Mutex<VecDeque<(String, String)>>,
    polls: Mutex<HashMap<String, Poll>>,
    numbered: Mutex<HashMap<String, Vec<String>>>,
}

/// A poll to open for an outgoing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PollRequest {
    pub(crate) question: String,
    pub(crate) options: Vec<String>,
    pub(crate) selectable: usize,
}

/// Whether a button answers a clarify question (approvals carry `appr:`).
fn clarify(button: &Button) -> bool {
    button.data.starts_with("clar:")
}

impl Session {
    /// A device in `mode` admitting groups by `group_policy` (the
    /// approved ones are `allowed_groups`).
    pub(crate) fn new(
        mode: WhatsappMode,
        group_policy: GroupPolicy,
        allowed_groups: Vec<String>,
    ) -> Self {
        Self {
            mode,
            group_policy,
            allowed_groups,
            ..Self::default()
        }
    }

    /// Remembers an own send.
    pub(crate) fn sent(&self, id: &str, text: &str) {
        let mut own = self.own.lock().expect("session poisoned");
        if own.len() >= OWN_SENDS {
            own.pop_front();
        }
        own.push_back((id.to_owned(), text.to_owned()));
    }

    fn own_text(&self, id: &str) -> Option<String> {
        self.own
            .lock()
            .expect("session poisoned")
            .iter()
            .find(|(own, _)| own == id)
            .map(|(_, text)| text.clone())
    }

    /// The poll a message's choices become: a multi-select control, or
    /// the buttons of a clarify question; `None` for anything else.
    pub(crate) fn poll_for(
        question: &str,
        buttons: &[Button],
        select: Option<&Select>,
    ) -> Option<PollRequest> {
        let question = question
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("?");
        if let Some(select) = select {
            return Some(PollRequest {
                question: question.to_owned(),
                options: select.options.clone(),
                selectable: select.options.len(),
            });
        }
        (!buttons.is_empty() && buttons.iter().all(clarify)).then(|| PollRequest {
            question: question.to_owned(),
            options: buttons.iter().map(|b| b.label.clone()).collect(),
            selectable: 1,
        })
    }

    /// Remembers poll `id` opened for `buttons` or `select`.
    pub(crate) fn opened(
        &self,
        id: &str,
        request: &PollRequest,
        buttons: &[Button],
        select: Option<&Select>,
    ) {
        let answer = match select {
            Some(select) => PollAnswer::Select(select.data.clone()),
            None => PollAnswer::Buttons(buttons.iter().map(|b| b.data.clone()).collect()),
        };
        self.polls.lock().expect("session poisoned").insert(
            id.to_owned(),
            Poll {
                options: request.options.clone(),
                answer,
            },
        );
    }

    /// The text of an approval card with its choices numbered (approvals
    /// are answered by number, never by poll); remembers the choices.
    pub(crate) fn numbered(&self, chat: &str, text: &str, buttons: &[Button]) -> String {
        let lines: Vec<String> = buttons
            .iter()
            .enumerate()
            .map(|(i, b)| format!("{}. {}", i + 1, b.label))
            .collect();
        self.numbered.lock().expect("session poisoned").insert(
            chat.to_owned(),
            buttons.iter().map(|b| b.data.clone()).collect(),
        );
        format!("{text}\n\nReply with a number:\n{}", lines.join("\n"))
    }

    /// Whether an event counts under the mode and the group policy.
    pub(crate) fn admits(&self, inbound: &Inbound, from_me: bool) -> bool {
        let echo = self.own_text(&inbound.message_id).is_some();
        let mode = match self.mode {
            WhatsappMode::Bot => !from_me,
            WhatsappMode::SelfChat => {
                from_me
                    && !echo
                    && inbound.chat_type == ChatType::Dm
                    && inbound.chat_id == inbound.sender_id
            }
        };
        let group = inbound.chat_type != ChatType::Group
            || match self.group_policy {
                GroupPolicy::Open => true,
                GroupPolicy::Allowlist | GroupPolicy::Pairing => {
                    self.allowed_groups.contains(&inbound.chat_id)
                }
            };
        mode && group
    }

    /// `inbound` with what the device knows added: a vote on one of its
    /// polls or a number answering its approval card becomes the
    /// card's callback, and a quote of its own send carries that text.
    pub(crate) fn enrich(&self, mut inbound: Inbound, raw: &Value) -> Inbound {
        inbound.addressing = self.addressing(&inbound, raw);
        if let Some(callback) = self.vote(raw).or_else(|| self.number(&inbound)) {
            inbound.callback = Some(callback);
            inbound.text.clear();
            return inbound;
        }
        let quoted = raw["quotedBody"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| inbound.reply_to.as_deref().and_then(|id| self.own_text(id)));
        if let Some(quoted) = quoted.filter(|q| !q.trim().is_empty()) {
            inbound.text = format!("[Replying to: {}]\n{}", quoted.trim(), inbound.text);
        }
        inbound
    }

    /// How a message addresses this device: the bridge lists it among the
    /// mentioned ids (`mentionedIds`, with `botIds` naming the device's own
    /// ids, as the Hermes bridge sends them), or it quotes one of the
    /// device's sends (`quotedParticipant`, or a remembered own send).
    fn addressing(&self, inbound: &Inbound, raw: &Value) -> pmt_gateway::messaging::Addressing {
        let own: Vec<&str> = raw["botIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let mentioned = raw["mentionedIds"].as_array().is_some_and(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .any(|id| own.contains(&id))
        });
        let quoted = raw["quotedParticipant"].as_str().unwrap_or_default();
        let replies_to_bot = (!quoted.is_empty() && own.contains(&quoted))
            || inbound
                .reply_to
                .as_deref()
                .is_some_and(|id| self.own_text(id).is_some());
        pmt_gateway::messaging::Addressing {
            mentioned,
            replies_to_bot,
            ..pmt_gateway::messaging::Addressing::default()
        }
    }

    fn vote(&self, raw: &Value) -> Option<String> {
        if raw["mediaType"] != "poll_update" {
            return None;
        }
        let id = raw["quotedMessageId"].as_str()?;
        let poll = self
            .polls
            .lock()
            .expect("session poisoned")
            .get(id)
            .cloned()?;
        let picked: Vec<usize> = raw["pollUpdate"]["selectedOptions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| o.as_str())
            .filter_map(|o| poll.options.iter().position(|p| p == o))
            .map(|i| i + 1)
            .collect();
        match poll.answer {
            PollAnswer::Select(data) if !picked.is_empty() => {
                Some(Select::submission(&data, &picked))
            }
            PollAnswer::Buttons(data) => data.get(picked.first()?.checked_sub(1)?).cloned(),
            PollAnswer::Select(_) => None,
        }
    }

    fn number(&self, inbound: &Inbound) -> Option<String> {
        let n: usize = inbound.text.trim().parse().ok()?;
        let mut numbered = self.numbered.lock().expect("session poisoned");
        let data = numbered
            .get(&inbound.chat_id)?
            .get(n.checked_sub(1)?)?
            .clone();
        numbered.remove(&inbound.chat_id);
        Some(data)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn buttons(prefix: &str) -> Vec<Button> {
        ["red", "green", "blue"]
            .iter()
            .map(|c| Button {
                label: (*c).into(),
                data: format!("{prefix}:id:0.{c}:mac"),
            })
            .collect()
    }

    #[test]
    fn a_clarify_question_is_a_poll_and_a_vote_answers_it() {
        let session = Session::default();
        let request = Session::poll_for("Which colour?\n1. red", &buttons("clar"), None).unwrap();
        assert_eq!(request.question, "Which colour?");
        assert_eq!(request.options, ["red", "green", "blue"]);
        assert_eq!(request.selectable, 1);
        session.opened("P1", &request, &buttons("clar"), None);
        let vote = json!({"mediaType": "poll_update", "quotedMessageId": "P1",
                          "pollUpdate": {"selectedOptions": ["green"]}});
        let inbound = session.enrich(Inbound::text(ChatType::Dm, "u", "u", "green"), &vote);
        assert_eq!(inbound.callback.as_deref(), Some("clar:id:0.green:mac"));
        assert!(Session::poll_for("Run it?", &buttons("appr"), None).is_none());
    }

    #[test]
    fn a_multi_select_poll_returns_every_vote() {
        let session = Session::default();
        let select = Select {
            data: "clar:id:0.m:mac".into(),
            options: vec!["mon".into(), "tue".into(), "wed".into()],
        };
        let request = Session::poll_for("Which days?", &[], Some(&select)).unwrap();
        assert_eq!(request.selectable, 3);
        session.opened("P2", &request, &[], Some(&select));
        let vote = json!({"mediaType": "poll_update", "quotedMessageId": "P2",
                          "pollUpdate": {"selectedOptions": ["mon", "wed"]}});
        let inbound = session.enrich(Inbound::text(ChatType::Dm, "u", "u", ""), &vote);
        assert_eq!(inbound.callback.as_deref(), Some("clar:id:0.m:mac|1,3"));
    }

    #[test]
    fn approvals_are_numbered_and_answered_by_number() {
        let session = Session::default();
        let text = session.numbered("c1", "Run rm?", &buttons("appr"));
        assert!(text.ends_with("1. red\n2. green\n3. blue"), "{text}");
        let reply = session.enrich(Inbound::text(ChatType::Dm, "c1", "c1", "2"), &json!({}));
        assert_eq!(reply.callback.as_deref(), Some("appr:id:0.green:mac"));
        let again = session.enrich(Inbound::text(ChatType::Dm, "c1", "c1", "2"), &json!({}));
        assert!(again.callback.is_none(), "answered once");
    }

    #[test]
    fn self_chat_mode_admits_only_the_users_own_chat() {
        let session = Session {
            mode: WhatsappMode::SelfChat,
            ..Session::default()
        };
        let me = "15550001111@s.whatsapp.net";
        let note = Inbound::text(ChatType::Dm, me, me, "remind me");
        assert!(session.admits(&note, true));
        let contact = Inbound::text(
            ChatType::Dm,
            "1999@s.whatsapp.net",
            "1999@s.whatsapp.net",
            "hi",
        );
        assert!(!session.admits(&contact, false));
        let mut echo = Inbound::text(ChatType::Dm, me, me, "Noted.");
        echo.message_id = "OWN1".into();
        session.sent("OWN1", "Noted.");
        assert!(!session.admits(&echo, true));
        let bot = Session::default();
        assert!(bot.admits(&contact, false));
        assert!(!bot.admits(&note, true));
    }

    #[test]
    fn groups_need_approval_unless_open_and_quotes_of_own_sends_are_resolved() {
        let session = Session::default();
        let group = Inbound::text(ChatType::Group, "123@g.us", "1@s.whatsapp.net", "hi");
        assert!(
            !session.admits(&group, false),
            "pairing admits no group until approved"
        );
        let approved = Session {
            allowed_groups: vec!["123@g.us".into()],
            ..Session::default()
        };
        assert!(approved.admits(&group, false));
        let open = Session {
            group_policy: GroupPolicy::Open,
            ..Session::default()
        };
        assert!(open.admits(&group, false));
        session.sent("OWN9", "The report is ready.");
        let mut reply = Inbound::text(ChatType::Dm, "u", "u", "thanks");
        reply.reply_to = Some("OWN9".into());
        let enriched = session.enrich(reply, &json!({}));
        assert_eq!(enriched.text, "[Replying to: The report is ready.]\nthanks");
    }
}
