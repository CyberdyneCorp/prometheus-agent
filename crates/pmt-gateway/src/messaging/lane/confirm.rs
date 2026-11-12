//! Restart and update from a chat (spec: messaging-gateway, Restart and
//! update from chat; distribution-and-update, Update from chat). Both are
//! admin-tier and ask for a confirmation in the same chat and thread first
//! (a card with Confirm/Cancel buttons where the platform has them,
//! `/approve` or `/deny` everywhere); only the requester or an admin may
//! answer, and a denial or no answer within `approvals.timeout_secs`
//! aborts without touching anything.
//!
//! - `/restart` drains the daemon and restarts it under its supervisor
//!   (exit 75); the next daemon posts "Gateway restarted (<build>)" to the
//!   requesting chat and thread.
//! - `/update [check]` needs `update.allow_from_chat = true` in
//!   `daemon.toml`; `check` only looks for a release and asks nothing.
//!   A confirmed update runs `prometheus update` detached; its stages are
//!   relayed here ([`super::update_relay`]), a busy daemon's restart is
//!   asked here again, and the restarted daemon says `updated to <v>`.

use std::time::Duration;

use jiff::Zoned;
use pmt_store::gateway::outbox::OutboundKind;
use serde_json::json;
use tokio::time::Instant;

use super::Lane;
use crate::messaging::adapter::{Button, Outgoing};
use crate::messaging::agents::{UpdateFromChat, UpdateRun};
use crate::messaging::auth;
use crate::messaging::cards::{Card, CardKind};
use crate::messaging::descriptor::Interactive;
use crate::messaging::key::Routing;
use crate::messaging::restart;

/// What a confirmation is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Restart,
    Update,
    /// Restart a daemon whose turns still run, once they finish.
    UpdateBusy,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::Update | Self::UpdateBusy => "update",
        }
    }
}

/// A confirmation waiting for its answer.
#[derive(Debug, Clone)]
pub(crate) struct Confirm {
    id: String,
    action: Action,
    requester: String,
    card_key: String,
    deadline: Instant,
}

impl Confirm {
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
}

/// How an unanswered confirmation ended (`why` is a catalog key).
fn aborted(strings: &pmt_i18n::Strings, action: Action, why: &str) -> String {
    let why = strings.get(why);
    pmt_i18n::t!(
        strings,
        "gateway.aborted",
        action = action.name(),
        why = why
    )
}

impl Lane {
    /// `/restart`.
    pub(super) fn restart_command(&mut self, routing: &Routing) {
        let drain = self.shared.options.drain_timeout.as_secs();
        let question = pmt_i18n::t!(self.strings(), "gateway.restart_question", secs = drain);
        self.ask_confirmation(routing, Action::Restart, &question);
    }

    /// `/update [check]`.
    pub(super) async fn update_command(&mut self, routing: &Routing, args: &str) {
        let allowed = pmt_config::daemon::DaemonConfig::load(&self.shared.options.home)
            .is_ok_and(|daemon| daemon.update.allow_from_chat);
        if !allowed {
            self.say(self.strings().get("gateway.updates_disabled"));
            return;
        }
        match args.trim() {
            "check" => self.run_update(routing, true, false).await,
            "" => {
                let question = pmt_i18n::t!(self.strings(), "gateway.update_question");
                self.ask_confirmation(routing, Action::Update, &question);
            }
            _ => self.say(self.strings().get("gateway.update_usage")),
        }
    }

    fn ask_confirmation(&mut self, routing: &Routing, action: Action, question: &str) {
        let id = crate::tickets::random_hex(8);
        let timeout = Duration::from_secs(
            self.config()
                .map_or(300, |s| s.config().approvals.timeout_secs)
                .max(1),
        );
        let native = self.tier() == Interactive::Native;
        let strings = self.strings();
        let card = Card {
            title: question.to_owned(),
            details: if native {
                Vec::new()
            } else {
                vec![pmt_i18n::t!(strings, "gateway.confirm_hint")]
            },
            deadline_ms: u64::try_from(timeout.as_millis()).ok(),
            on_expiry: pmt_i18n::t!(strings, "gateway.confirm_on_expiry"),
        };
        let buttons: Vec<Button> = if native {
            [
                ("gateway.confirm_button", "yes"),
                ("gateway.cancel_button", "no"),
            ]
            .iter()
            .map(|(label, choice)| Button {
                label: strings.get(label).to_owned(),
                data: self.shared.card_key.data(CardKind::Confirm, &id, choice),
            })
            .collect()
        } else {
            Vec::new()
        };
        let card_key = format!("confirm:{id}");
        let text = card.text(&strings, &Zoned::now());
        let body = json!({"text": text, "buttons": buttons}).to_string();
        self.deliver(card_key.clone(), OutboundKind::Approval, body, None);
        self.confirms.push(Confirm {
            id,
            action,
            requester: routing.sender_id.clone(),
            card_key,
            deadline: Instant::now() + timeout,
        });
    }

    /// A Confirm/Cancel button.
    pub(super) fn confirm_pressed(&mut self, routing: &Routing, id: &str, choice: &str) {
        let Some(at) = self.confirms.iter().position(|c| c.id == id) else {
            tracing::info!("ignored an answer to an unknown or settled confirmation");
            return;
        };
        if !self.may_confirm(routing, &self.confirms[at]) {
            self.say_privately(routing, self.strings().get("gateway.confirm_not_yours"));
            return;
        }
        let confirm = self.confirms.remove(at);
        self.settle(routing, &confirm, choice == "yes");
    }

    /// `/approve` or `/deny` with no approval pending: the oldest
    /// confirmation the sender may answer; `false` when there is none.
    pub(super) fn answer_confirmation(&mut self, routing: &Routing, yes: bool) -> bool {
        let Some(at) = self
            .confirms
            .iter()
            .position(|c| self.may_confirm(routing, c))
        else {
            return false;
        };
        let confirm = self.confirms.remove(at);
        self.settle(routing, &confirm, yes);
        true
    }

    fn may_confirm(&self, routing: &Routing, confirm: &Confirm) -> bool {
        confirm.requester == routing.sender_id
            || self.config().is_some_and(|scope| {
                let table = scope.config().gateway.platform(&routing.platform);
                auth::is_admin(&table, routing.chat_type, &routing.sender_id) == Some(true)
            })
    }

    fn settle(&mut self, routing: &Routing, confirm: &Confirm, yes: bool) {
        let strings = self.strings();
        let outcome = if yes {
            pmt_i18n::t!(
                strings,
                "gateway.confirmed_by",
                action = confirm.action.name(),
                who = routing.sender_id
            )
        } else {
            aborted(&strings, confirm.action, "gateway.confirm_denied")
        };
        self.close_confirmation(confirm, &outcome);
        if !yes {
            tracing::info!(action = confirm.action.name(), "confirmation denied");
            return;
        }
        let routing = routing.clone();
        let action = confirm.action;
        let tx = self.tx.clone();
        // The action runs on the lane's task once this message is handled.
        if let Some(tx) = tx.upgrade() {
            let _ = tx.send(super::Msg::Confirmed {
                action,
                routing: Box::new(routing),
            });
        }
    }

    /// Replaces the card with its outcome (in place where possible).
    fn close_confirmation(&self, confirm: &Confirm, outcome: &str) {
        let message_id = self
            .shared
            .store(&self.profile)
            .and_then(|store| store.outbound_message_id(&confirm.card_key).ok().flatten());
        let body = json!({"text": outcome, "buttons": []}).to_string();
        let key = format!("{}:outcome", confirm.card_key);
        self.deliver(key, OutboundKind::Approval, body, message_id);
    }

    /// Cancels confirmations nobody answered in time.
    pub(super) fn expire_confirmations(&mut self, now: Instant) {
        let (expired, waiting): (Vec<Confirm>, Vec<Confirm>) =
            self.confirms.drain(..).partition(|c| c.deadline <= now);
        self.confirms = waiting;
        let strings = self.strings();
        for confirm in expired {
            let text = aborted(&strings, confirm.action, "gateway.confirm_timeout");
            self.close_confirmation(&confirm, &text);
        }
    }

    /// Runs a confirmed action.
    pub(super) async fn confirmed(&mut self, action: Action, routing: &Routing) {
        match action {
            Action::Restart => self.restart_now(routing).await,
            Action::Update => self.run_update(routing, false, false).await,
            Action::UpdateBusy => self.run_update(routing, false, true).await,
        }
    }

    /// The updater stopped before the swap: the daemon has turns running.
    pub(super) fn ask_busy_restart(&mut self, routing: &Routing, question: &str) {
        let question = pmt_i18n::t!(
            self.strings(),
            "gateway.update_not_applied_yet",
            question = question
        );
        self.ask_confirmation(routing, Action::UpdateBusy, &question);
    }

    async fn restart_now(&self, routing: &Routing) {
        let requester = restart::Requester {
            profile: self.profile.clone(),
            platform: routing.platform.clone(),
            chat_id: routing.chat_id.clone(),
            thread_id: routing.thread_id.clone(),
        };
        restart::remember(&self.shared.options.home, &requester);
        // Said directly: the outbox stops with the daemon.
        let text = pmt_i18n::t!(self.strings(), "gateway.restarting");
        let message = Outgoing::text(&routing.chat_id, routing.thread_id.as_deref(), &text);
        if let Err(err) = self
            .shared
            .adapters
            .send_direct(&self.profile, &routing.platform, &message)
            .await
        {
            tracing::warn!("restart notice not sent: {}", err.describe());
        }
        tracing::info!(
            profile = self.profile,
            platform = routing.platform,
            "restart requested from a chat"
        );
        self.shared.server.request_stop(true);
    }

    async fn run_update(&self, routing: &Routing, check: bool, restart_busy: bool) {
        let request = UpdateFromChat {
            profile: self.profile.clone(),
            platform: routing.platform.clone(),
            chat_id: routing.chat_id.clone(),
            thread_id: routing.thread_id.clone(),
            check,
            restart_busy,
        };
        match self.shared.agents.update_from_chat(&request).await {
            Ok(UpdateRun::Answer(text)) => self.say(&text),
            Ok(UpdateRun::Running(feed)) => {
                self.say(self.strings().get("gateway.update_started"));
                super::update_relay::spawn(
                    &self.shared,
                    self.tx.clone(),
                    routing.clone(),
                    feed,
                    self.strings(),
                );
            }
            Err(err) => self.say(err.message()),
        }
    }
}
