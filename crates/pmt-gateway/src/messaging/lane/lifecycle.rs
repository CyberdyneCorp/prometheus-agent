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

impl Lane {
    /// Marks the triggering message `message_id` with `emoji`.
    pub(super) async fn mark(&self, message_id: Option<&String>, emoji: &str) {
        let platform = &self.routing.platform;
        let on = self
            .config()
            .is_some_and(|s| s.config().gateway.platform(platform).reactions_on(platform));
        let (Some(message_id), true) = (message_id, on) else {
            return;
        };
        let Some(slot) = self
            .shared
            .adapters
            .ready(&self.profile, platform)
            .filter(|s| s.can(Op::Reactions))
        else {
            return;
        };
        let reaction = Reaction {
            chat_id: self.routing.chat_id.clone(),
            message_id: message_id.clone(),
            emoji: emoji.to_owned(),
            remove: false,
        };
        match slot.adapter.react(&reaction).await {
            Ok(()) => {}
            Err(SendError::Unsupported) => slot.refused(Op::Reactions),
            Err(err) => tracing::debug!("lifecycle reaction not set: {}", err.describe()),
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
        adapter
            .thread_titled(&self.routing.chat_id, &thread, &title)
            .await;
    }
}
