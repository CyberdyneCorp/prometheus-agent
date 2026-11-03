//! Observe mode (spec: messaging-gateway, Group mention gating and observe
//! mode): authorized group messages that do not address the bot are kept
//! (per chat, see `gating::Observed`) and appended to the session of the
//! chat's next turn as attributed `[name|user_id]` user rows, right before
//! its message; that turn's model input carries a notice that those lines
//! are context, not instructions, never stored in its text.

use jiff::Timestamp;
use pmt_store::{NewMessage, Role};
use serde_json::json;

use super::Lane;
use crate::messaging::gating::{self, OBSERVED_NOTICE};
use crate::messaging::key::Routing;

impl Lane {
    /// Appends the chat's observed lines to `session` before a turn.
    pub(super) fn add_observed(&mut self, session: &str, routing: &Routing) {
        let chat = gating::chat_key(
            &self.profile,
            &routing.platform,
            &routing.chat_id,
            routing.thread_id.as_deref(),
        );
        let lines = self.shared.observed.take(&chat);
        if lines.is_empty() {
            return;
        }
        let (Some(store), Ok(id)) = (self.shared.store(&self.profile), session.parse()) else {
            return;
        };
        for line in lines {
            let mut row = NewMessage::new(Role::User, &line);
            row.metadata = Some(json!({"observed": true}));
            if let Err(err) = store.append_message(id, &row) {
                tracing::warn!("observed message not kept: {err}");
            }
        }
        self.observed_since_turn = true;
    }

    /// What the model sees in front of a turn's text: the observed-lines
    /// notice after observed lines, and the send time in a session created
    /// with message timestamps.
    pub(super) fn model_prefix(&mut self, session: &str, sent_at: Timestamp) -> Option<String> {
        let notice = std::mem::take(&mut self.observed_since_turn).then_some(OBSERVED_NOTICE);
        let stamp = self.timestamp_prefix(session, sent_at);
        match (notice, stamp) {
            (Some(notice), Some(stamp)) => Some(format!("{notice}\n\n{stamp}")),
            (Some(notice), None) => Some(notice.to_owned()),
            (None, stamp) => stamp,
        }
    }
}
