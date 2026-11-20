//! Processing lifecycle reactions (spec: platform-adapters, Processing
//! lifecycle reactions): where the platform's `reactions` setting is on
//! (default on Discord, Slack, Feishu and `DingTalk`) and the adapter
//! supports reactions, the triggering message gets `eyes` when its turn
//! starts and a check mark or a cross when it ends. A platform that keeps
//! one bot reaction per message (Telegram) replaces the first with the
//! second. A failed reaction never affects delivery.

use pmt_rpc::contracts::TurnFinished;

use super::Lane;
use crate::messaging::adapter::{Reaction, SendError};
use crate::messaging::adapters::Op;

/// A turn started.
pub(super) const STARTED: &str = "\u{1f440}";
/// It answered.
pub(super) const SUCCEEDED: &str = "\u{2705}";
/// It failed.
pub(super) const FAILED: &str = "\u{274c}";

/// The reaction a finished turn earns.
pub(super) fn outcome(finished: &TurnFinished) -> &'static str {
    let completed = finished.error.is_none()
        && matches!(
            finished.exit_reason.as_str(),
            "completed" | "interrupted_by_user"
        );
    if completed { SUCCEEDED } else { FAILED }
}

/// The emoji of a reaction setting: a name (`eyes`, `thumbsup`, ...) or
/// the emoji itself.
pub(super) fn emoji(setting: &str) -> &str {
    match setting.trim().trim_matches(':') {
        "eyes" => STARTED,
        "thumbsup" | "+1" | "thumbs_up" => "\u{1f44d}",
        "hourglass" => "\u{23f3}",
        "white_check_mark" | "check" => SUCCEEDED,
        "ok_hand" => "\u{1f44c}",
        "inbox_tray" => "\u{1f4e5}",
        "speech_balloon" => "\u{1f4ac}",
        other => other,
    }
}

impl Lane {
    /// Marks the triggering message `message_id` with `emoji`.
    pub(super) async fn mark(&self, message_id: Option<&String>, emoji: &str) {
        let platform = &self.routing.platform;
        let on = self
            .config()
            .is_some_and(|s| s.config().gateway.platform(platform).reactions_on(platform));
        if let (Some(message_id), true) = (message_id, on) {
            self.react(message_id, emoji).await;
        }
    }

    /// Adds `emoji` to message `message_id` of this chat; `false` when the
    /// adapter has no reactions or the platform refused it.
    pub(super) async fn react(&self, message_id: &str, emoji: &str) -> bool {
        let Some(slot) = self
            .shared
            .adapters
            .ready(&self.profile, &self.routing.platform)
            .filter(|s| s.can(Op::Reactions))
        else {
            return false;
        };
        let reaction = Reaction {
            chat_id: self.routing.chat_id.clone(),
            message_id: message_id.to_owned(),
            emoji: emoji.to_owned(),
            remove: false,
        };
        match slot.adapter.react(&reaction).await {
            Ok(()) => true,
            Err(SendError::Unsupported) => {
                slot.refused(Op::Reactions);
                false
            }
            Err(err) => {
                tracing::debug!("reaction not set: {}", err.describe());
                false
            }
        }
    }

    /// Spec: platform-adapters, Busy acknowledgement via reactions: a
    /// follow-up during a turn gets the platform's `busy_reaction`
    /// (default `eyes`) instead of a busy text; `false` when it could not.
    pub(super) async fn react_busy(&self, message_id: Option<&String>) -> bool {
        let Some(message_id) = message_id else {
            return false;
        };
        let setting = self
            .config()
            .and_then(|s| {
                s.config()
                    .gateway
                    .platform(&self.routing.platform)
                    .busy_reaction
            })
            .unwrap_or_else(|| "eyes".to_owned());
        self.react(message_id, emoji(&setting)).await
    }

    /// `gateway.ack_reaction` on every accepted inbound message.
    pub(super) async fn ack_inbound(&self, message_id: &str) {
        let setting = self
            .config()
            .and_then(|s| s.config().gateway.ack_reaction.clone());
        if let (Some(setting), false) = (setting, message_id.is_empty()) {
            self.react(message_id, emoji(&setting)).await;
        }
    }
}

impl Lane {
    /// After the first turn of a thread's session, offers the session's
    /// title to the adapter (a thread it opened may take it as its name).
    pub(super) async fn offer_title(&mut self, session: &str) {
        let Some(thread) = self.routing.thread_id.clone() else {
            return;
        };
        if self.titled.as_deref() == Some(session) {
            return;
        }
        self.titled = Some(session.to_owned());
        let title = self
            .shared
            .store(&self.profile)
            .zip(session.parse().ok())
            .and_then(|(store, id)| store.session(id).ok())
            .and_then(|s| s.title);
        let (Some(title), Some(adapter)) = (
            title,
            self.shared
                .adapters
                .adapter(&self.profile, &self.routing.platform),
        ) else {
            return;
        };
        self.name_default_topic(&title).await;
        adapter
            .thread_titled(&self.routing.chat_id, &thread, &title)
            .await;
    }
}
