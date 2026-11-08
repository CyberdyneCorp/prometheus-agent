//! Compaction notices and the context pressure indicator in a chat (spec:
//! context-compression, Compaction progress notices and context pressure
//! gauge).
//!
//! A chat is a shared, quiet place: routine automatic compaction statuses
//! (start, retry, completion, idle compaction) are posted only with
//! `compression.progress_notices = true`, while failures always are (a
//! manual `/compress` answers through its own reply). The context
//! pressure indicator is one plain-text notice each time the session's
//! context crosses into a higher level (60% and 85% of the compaction
//! trigger), never a repeated gauge; it is never added to the model's
//! context.

use pmt_core::context_pressure::{Level, compaction_notice, notice, of_turn_end};
use pmt_store::gateway::outbox::OutboundKind;
use serde_json::Value;

use super::Lane;

impl Lane {
    /// Shows what a compaction lifecycle event or a turn's context
    /// pressure means for this chat.
    pub(super) fn compaction_event(&mut self, kind: &str, payload: &Value) {
        if kind == "turn.end" {
            self.pressure(payload);
            return;
        }
        let Some(notice) = compaction_notice(kind, payload) else {
            return;
        };
        let progress = self
            .config()
            .is_some_and(|s| s.config().compression.progress_notices);
        if notice.routine && !progress {
            return;
        }
        let as_kind = if notice.routine {
            OutboundKind::System
        } else {
            OutboundKind::Warning
        };
        self.say_as(as_kind, &notice.text, None);
    }

    /// One notice per level crossing per session.
    fn pressure(&mut self, payload: &Value) {
        let Some((percent, level)) = of_turn_end(payload) else {
            return;
        };
        let session = self.session.clone().unwrap_or_default();
        let before = self
            .pressure_level
            .as_ref()
            .filter(|(s, _)| *s == session)
            .and_then(|(_, l)| *l);
        self.pressure_level = Some((session, level));
        let Some(level) = level.filter(|l| before.is_none_or(|b| l > &b)) else {
            return;
        };
        let as_kind = match level {
            Level::Info => OutboundKind::System,
            Level::Warning => OutboundKind::Warning,
        };
        self.say_as(as_kind, &notice(level, percent), None);
    }
}
