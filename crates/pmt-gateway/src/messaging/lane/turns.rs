//! The conversation's session and turns (spec: messaging-gateway,
//! Conversations never rotate on time; Single-flight session routing;
//! Streaming to platforms; Durable delivery outbox; Stuck-loop
//! suspension). The session a key routes to is read from the store at
//! every turn and replaced only by `/new`, a suspension, or a route found
//! stale (pruned, or its session ended); never by elapsed time.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{PromptImage, SubmitParams, TurnFinished};
use pmt_store::gateway::outbox::OutboundKind;
use pmt_store::gateway::routes::{NewRoute, ResetReason};
use serde_json::json;
use tokio::time::Instant;

use super::{Deferred, Lane, Msg, Origin, Pending, Running};
use crate::host::Author;
use crate::messaging::adapter::Outgoing;
use crate::messaging::agents::Conversation;
use crate::messaging::key::Routing;
use crate::messaging::peer;

/// Told once when a suspended session is replaced.
pub(crate) const SUSPENDED_NOTICE: &str = "Your previous session was suspended after repeated \
    restarts while it was working; this is a fresh session.";

/// A streamed answer being shown as one edited message.
#[derive(Debug, Default)]
pub(crate) struct Draft {
    text: String,
    message_id: Option<String>,
    last_edit: Option<Instant>,
    failed: bool,
}

impl Lane {
    /// Records human activity on the route.
    pub(super) fn touch(&self) {
        if let Some(store) = self.shared.store(&self.profile)
            && let Err(err) = store.touch_route(self.key.as_str(), Timestamp::now())
        {
            tracing::warn!("route activity not recorded: {err}");
        }
    }

    /// The session the key routes to now, if any.
    pub(super) fn current_session(&self) -> Option<String> {
        let store = self.shared.store(&self.profile)?;
        store
            .route(self.key.as_str())
            .ok()
            .flatten()
            .map(|r| r.session_id.to_string())
    }

    /// The key's session, created or replaced when needed: a suspended
    /// session or an ended one is replaced (`suspended`, `stale_route`),
    /// and a key whose route was pruned starts over (`stale_route`).
    pub(super) async fn ensure_session(&mut self, routing: &Routing) -> Result<String, Error> {
        let store = self
            .shared
            .store(&self.profile)
            .ok_or_else(|| Error::new(&codes::PROFILE_NOT_FOUND, "the profile is not served"))?;
        let Some(route) = store.route(self.key.as_str())? else {
            let previous = store.latest_routed_session(self.key.as_str())?;
            let reason = previous.map(|_| ResetReason::StaleRoute);
            return self
                .transition(routing, reason, previous.map(|p| p.to_string()))
                .await;
        };
        let id = route.session_id;
        let suspended = store.routed_session(id)?.is_some_and(|s| s.suspended);
        if suspended {
            let session = self
                .transition(routing, Some(ResetReason::Suspended), Some(id.to_string()))
                .await?;
            self.say(SUSPENDED_NOTICE);
            return Ok(session);
        }
        let ended = store.session(id).map_or(true, |s| s.ended_at.is_some());
        if ended {
            return self
                .transition(routing, Some(ResetReason::StaleRoute), Some(id.to_string()))
                .await;
        }
        let session = id.to_string();
        self.adopt(&session);
        Ok(session)
    }

    fn adopt(&mut self, session: &str) {
        if self.session.as_deref() != Some(session) {
            peer::attach(&self.shared, &self.profile, session, self.key.as_str());
            self.session = Some(session.to_owned());
        }
    }

    /// Creates the key's next session and routes the key to it. The
    /// replaced session ends (`/new`, `/reset`, suspension); a stale
    /// route's session had already ended elsewhere.
    pub(super) async fn transition(
        &mut self,
        routing: &Routing,
        reason: Option<ResetReason>,
        parent: Option<String>,
    ) -> Result<String, Error> {
        let conversation = Conversation {
            platform: routing.platform.clone(),
            chat_id: routing.chat_id.clone(),
            thread_id: routing.thread_id.clone(),
            user_id: routing.sender_id.clone(),
            parent: parent.clone(),
        };
        let agents = &self.shared.agents;
        let session = agents
            .create_conversation(&self.profile, conversation)
            .await?;
        let store = self
            .shared
            .store(&self.profile)
            .ok_or_else(|| Error::new(&codes::PROFILE_NOT_FOUND, "the profile is not served"))?;
        let route = NewRoute {
            key: self.key.as_str().to_owned(),
            session_id: session.parse()?,
            platform: routing.platform.clone(),
            chat_id: routing.chat_id.clone(),
            thread_id: routing.thread_id.clone(),
        };
        store.bind_route(&route, reason)?;
        self.adopt(&session);
        self.horizon = self.status.seq.load(std::sync::atomic::Ordering::Acquire);
        if let (Some(old), Some(reason)) = (parent.as_deref(), reason)
            && reason != ResetReason::StaleRoute
        {
            self.end_replaced(old, &session, routing, reason).await;
        }
        let hooks = &self.shared.options.hooks;
        let payload = json!({
            "session_id": session,
            "platform": routing.platform,
            "reset_reason": reason.map(ResetReason::as_str),
        });
        hooks
            .emit(&self.profile, "on_session_start", payload.clone())
            .await;
        if matches!(reason, Some(ResetReason::New | ResetReason::Reset)) {
            hooks.emit(&self.profile, "on_session_reset", payload).await;
        }
        Ok(session)
    }

    /// Ends `old`, replaced by `session`, and runs the profile's
    /// `on_session_end` hooks; a session that could not be ended keeps
    /// running, so no hook fires for it.
    async fn end_replaced(&self, old: &str, session: &str, routing: &Routing, reason: ResetReason) {
        let agents = &self.shared.agents;
        if let Err(err) = agents.end_conversation(&self.profile, old).await {
            tracing::warn!("the replaced session did not end: {err}");
            return;
        }
        let payload = json!({
            "session_id": old,
            "platform": routing.platform,
            "reset_reason": reason.as_str(),
            "replaced_by": session,
        });
        let hooks = &self.shared.options.hooks;
        hooks.emit(&self.profile, "on_session_end", payload).await;
    }

    /// Starts a turn with `pending` in the key's session.
    pub(super) async fn start_turn(&mut self, pending: Pending) {
        let session = match self.ensure_session(&pending.routing).await {
            Ok(session) => session,
            Err(err) => {
                self.say(err.message());
                return;
            }
        };
        let store = self.shared.store(&self.profile);
        if let Some(store) = &store
            && let Ok(id) = session.parse()
            && let Err(err) = store.set_turn_active(id, true)
        {
            tracing::warn!("turn not recorded as running: {err}");
        }
        self.shared.cache.touch(&self.profile, &session);
        let author = Author {
            client_kind: "gateway".to_owned(),
            client_id: format!("{}:{}", pending.routing.platform, pending.routing.sender_id),
            principal: self.profile.clone(),
        };
        let params = SubmitParams {
            session_id: session.clone(),
            text: pending.text.clone(),
            images: pending
                .images
                .iter()
                .map(|p| PromptImage::Path(p.clone()))
                .collect(),
            run_budget_secs: None,
            input_id: None,
            author: None,
        };
        let link = self.link(&session, &pending.routing);
        self.shared
            .options
            .hooks
            .emit(
                &self.profile,
                "on_turn_start",
                json!({"session_id": session}),
            )
            .await;
        match self
            .shared
            .agents
            .submit(&self.profile, params, author, link)
            .await
        {
            Ok(submitted) => {
                self.running = Some(Running {
                    turn_id: submitted.turn_id,
                    requester: pending.routing.sender_id.clone(),
                    reply_to: pending.reply_to,
                    stopped: false,
                    draft: Draft::default(),
                });
            }
            Err(err) => {
                self.turn_over(&session);
                self.say(err.message());
            }
        }
    }

    /// The session's link; streaming platforms get the answer text as it
    /// is written.
    fn link(&self, session: &str, routing: &Routing) -> crate::host::Link {
        let link = self.shared.server.link(session);
        let streams = self
            .config()
            .is_some_and(|s| s.config().gateway.streams_on(&routing.platform))
            && self
                .shared
                .adapters
                .descriptor(&self.profile, &routing.platform)
                .is_some_and(|d| d.supports_edit);
        if !streams {
            return link;
        }
        let tx = self.tx.clone();
        link.with_deltas(Arc::new(move |text: &str| {
            if let Some(tx) = tx.upgrade() {
                let _ = tx.send(Msg::Delta(text.to_owned()));
            }
        }))
    }

    fn turn_over(&self, session: &str) {
        if let Some(store) = self.shared.store(&self.profile)
            && let Ok(id) = session.parse()
            && let Err(err) = store.set_turn_active(id, false)
        {
            tracing::warn!("turn not recorded as finished: {err}");
        }
    }

    /// A turn ended: its answer is delivered (unless `/stop` stopped it),
    /// then a deferred `/new` and the next waiting input run.
    pub(super) async fn finished(&mut self, finished: TurnFinished) {
        let ours = self
            .running
            .as_ref()
            .is_some_and(|r| r.turn_id == finished.turn_id);
        if !ours {
            return;
        }
        let Some(running) = self.running.take() else {
            return;
        };
        let session = self.session.clone().unwrap_or_default();
        self.turn_over(&session);
        if !running.stopped {
            self.deliver_answer(
                &session,
                &finished,
                &running.draft,
                running.reply_to.as_ref(),
            );
        }
        self.shared
            .options
            .hooks
            .emit(
                &self.profile,
                "on_turn_end",
                json!({"session_id": session, "exit_reason": finished.exit_reason}),
            )
            .await;
        if let Some(Deferred::New { reset, routing }) = self.deferred.take() {
            self.renew(&routing, reset).await;
        }
        self.next().await;
    }

    /// Runs what waits for the turn, in order, until one starts a turn.
    pub(super) async fn next(&mut self) {
        while self.running.is_none() {
            let Some(pending) = self.pending.pop_front() else {
                return;
            };
            match pending.origin {
                Origin::Human | Origin::Queued => self.start_turn(pending).await,
                Origin::Command => self.run_now(pending.routing, &pending.text).await,
                Origin::Internal => {
                    self.say_as(OutboundKind::Notification, &pending.text, None);
                }
            }
        }
    }

    fn deliver_answer(
        &self,
        session: &str,
        finished: &TurnFinished,
        draft: &Draft,
        reply_to: Option<&String>,
    ) {
        let text = match (&finished.final_response, &finished.error) {
            (Some(answer), _) if !answer.trim().is_empty() => answer.clone(),
            (_, Some(error)) => format!("The turn failed: {}", error.message),
            _ => return,
        };
        let limit = self
            .shared
            .adapters
            .descriptor(&self.profile, &self.routing.platform)
            .map_or(4096, |d| d.max_message_length);
        let parts = split(&text, limit);
        for (index, part) in parts.into_iter().enumerate() {
            let new = pmt_store::gateway::outbox::NewOutbound {
                idempotency_key: format!("{session}:{}:{index}", finished.turn_id),
                session_id: Some(session.to_owned()),
                platform: self.routing.platform.clone(),
                chat_id: self.routing.chat_id.clone(),
                thread_id: self.routing.thread_id.clone(),
                reply_to: if index == 0 { reply_to.cloned() } else { None },
                edit_of: if index == 0 {
                    draft.message_id.clone()
                } else {
                    None
                },
                kind: OutboundKind::Reply,
                body: part,
            };
            self.shared.deliver(&self.profile, &new);
        }
    }

    /// Streamed answer text: a draft message sent once enough text
    /// arrived, then edited at most every `edit_interval_ms`.
    pub(super) async fn delta(&mut self, text: &str) {
        let Some(scope) = self.config() else {
            return;
        };
        let streaming = scope.config().gateway.streaming.clone();
        let Some(running) = self.running.as_mut() else {
            return;
        };
        let draft = &mut running.draft;
        draft.text.push_str(text);
        if draft.failed {
            return;
        }
        let now = Instant::now();
        let due = match (&draft.message_id, draft.last_edit) {
            (None, _) => draft.text.chars().count() >= streaming.buffer_threshold as usize,
            (Some(_), Some(at)) => {
                now.duration_since(at) >= Duration::from_millis(streaming.edit_interval_ms)
            }
            (Some(_), None) => true,
        };
        if !due {
            return;
        }
        let message = Outgoing {
            chat_id: self.routing.chat_id.clone(),
            thread_id: self.routing.thread_id.clone(),
            reply_to: None,
            text: format!("{}{}", draft.text, streaming.cursor),
            buttons: Vec::new(),
        };
        let adapters = &self.shared.adapters;
        let outcome = match &draft.message_id {
            None => adapters
                .send_direct(&self.profile, &self.routing.platform, &message)
                .await
                .map(|sent| draft.message_id = sent.message_id),
            Some(id) => {
                adapters
                    .edit_direct(&self.profile, &self.routing.platform, id, &message)
                    .await
            }
        };
        draft.last_edit = Some(now);
        if outcome.is_err() || draft.message_id.is_none() {
            // The final answer is then sent as a message of its own.
            draft.failed = true;
            draft.message_id = None;
        }
    }
}

/// Splits `text` into parts of at most `limit` characters, preferring
/// line breaks (fence-aware splitting is the platform adapters').
pub(crate) fn split(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut parts = Vec::new();
    let mut rest = text;
    while rest.chars().count() > limit {
        let cut = rest
            .char_indices()
            .nth(limit)
            .map_or(rest.len(), |(at, _)| at);
        let at = rest[..cut]
            .rfind('\n')
            .filter(|at| *at > 0)
            .map_or(cut, |at| at + 1);
        parts.push(rest[..at].to_owned());
        rest = &rest[at..];
    }
    if !rest.is_empty() || parts.is_empty() {
        parts.push(rest.to_owned());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn a_long_answer_splits_at_lines_within_the_limit() {
        let text = "aaaa\nbbbb\ncccc";
        assert_eq!(split(text, 10), vec!["aaaa\nbbbb\n", "cccc"]);
        assert_eq!(split("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(split("short", 100), vec!["short"]);
        assert!(split("ééééé", 2).iter().all(|p| p.chars().count() <= 2));
    }
}
