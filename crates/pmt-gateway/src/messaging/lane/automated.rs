//! Automated turns in a conversation (spec: session-automation, Goal
//! messages on messaging platforms; Heartbeat ownership, recovery and
//! silence on the gateway). A goal continuation, loop wakeup or heartbeat
//! runs as an ordinary turn of the lane, so it arbitrates with the chat's
//! own input: it starts only while nothing runs and no one's input waits.
//! It quotes no message (the reply is posted to the chat or thread), and
//! a heartbeat turn is quiet: no typing indicator, tool progress or
//! streamed draft, and a `NO_REPLY`/`[SILENT]` answer sends nothing.

use super::{Lane, Origin, Pending};
use crate::messaging::automation::{AfterTurn, Automated};

impl Lane {
    /// Starts `automated` when the conversation is idle.
    pub(super) async fn automated(&mut self, automated: Automated) {
        let busy = self.running.is_some()
            || self.buffer.is_some()
            || self.pending.iter().any(|p| p.origin != Origin::Internal);
        if busy {
            tracing::debug!(
                origin = automated.origin,
                "automated turn dropped: the chat is busy"
            );
            return;
        }
        if let Some(notice) = &automated.notice {
            self.say(notice);
        }
        let quiet = automated.quiet();
        self.automating = Some(automated.origin);
        if automated.text.starts_with('/') {
            // A loop whose wakeup is a slash command runs the command; one
            // that sends a message starts it as the wakeup's turn.
            let routing = self.routing.clone();
            self.run_now(routing, &automated.text).await;
            self.automating = None;
            return;
        }
        let pending = Pending {
            origin: Origin::Queued,
            text: automated.text,
            images: Vec::new(),
            routing: self.routing.clone(),
            reply_to: None,
            voice: false,
            sent_at: jiff::Timestamp::now(),
            addressed: !quiet,
        };
        self.start_turn(pending).await;
        self.automating = None;
    }

    /// What the host decided when the lane's turn ended: its status lines,
    /// then a continuation unless someone's input waits (that input runs
    /// next and the host judges again after it).
    pub(super) async fn after_automation(&mut self) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let after = match self
            .shared
            .agents
            .take_automated(&self.profile, &session)
            .await
        {
            Ok(after) => after,
            Err(err) => {
                tracing::warn!("automation state not read: {err}");
                AfterTurn::default()
            }
        };
        for notice in &after.notices {
            self.say(notice);
        }
        if let Some(turn) = after.turn
            && self.running.is_none()
        {
            self.automated(turn).await;
        }
    }
}
