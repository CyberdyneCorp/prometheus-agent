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
    pub(super) text: String,
    pub(super) message_id: Option<String>,
    pub(super) last_edit: Option<Instant>,
    pub(super) failed: bool,
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
        // A session handed off to this chat continues here (spec:
        // messaging-gateway, Session handoff to a messaging platform).
        let parked = store.take_handoff(
            self.key.as_str(),
            &routing.platform,
            &routing.chat_id,
            routing.thread_id.as_deref(),
        )?;
        if let Some(id) = parked {
            if store.session(id)?.ended_at.is_some() {
                store.reopen_session(id)?;
            }
            let session = id.to_string();
            self.adopt(&session);
            return Ok(session);
        }
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

    pub(super) fn adopt(&mut self, session: &str) {
        if self.session.as_deref() != Some(session) {
            peer::attach(&self.shared, &self.profile, session, self.key.as_str());
            self.session = Some(session.to_owned());
            self.load_policy(session);
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
        let policy = self.session_policy(routing);
        let conversation = Conversation {
            platform: routing.platform.clone(),
            chat_id: routing.chat_id.clone(),
            thread_id: routing.thread_id.clone(),
            user_id: routing.sender_id.clone(),
            parent: parent.clone(),
            policy: policy.clone(),
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
        let timestamps = self
            .config()
            .is_some_and(|s| s.config().gateway.message_timestamps.enabled);
        store.set_session_timestamps(route.session_id, timestamps)?;
        self.adopt(&session);
        self.freeze_policy(&session, policy);
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

    /// Starts a turn with `pending` in the key's session; while the home
    /// is paused the input is refused with the pause notice.
    pub(super) async fn start_turn(&mut self, pending: Pending) {
        if let Some(pause) = pmt_core::pause::active(&self.shared.options.home) {
            tracing::info!(profile = self.profile, "turn refused: the home is paused");
            self.say(&pause.notice());
            return;
        }
        let session = match self.ensure_session(&pending.routing).await {
            Ok(session) => session,
            Err(err) => {
                self.say(err.message());
                return;
            }
        };
        if self.hold(&session, &pending) {
            return;
        }
        self.add_observed(&session, &pending.routing);
        self.record_start(&session, &pending.routing);
        self.shared.cache.touch(&self.profile, &session);
        let author = Author {
            client_kind: "gateway".to_owned(),
            client_id: format!("{}:{}", pending.routing.platform, pending.routing.sender_id),
            principal: self.profile.clone(),
            sent_at: Some(pending.sent_at.to_string()),
            prefix: self.model_prefix(&session, pending.sent_at),
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
        let link = self.link(&session, &pending.routing, pending.voice);
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
                let routing = &pending.routing;
                let typing = crate::messaging::typing::start(
                    &self.shared,
                    &self.profile,
                    &routing.platform,
                    &routing.chat_id,
                    routing.thread_id.as_deref(),
                );
                let speech = self
                    .streams_audio(pending.voice)
                    .then(|| super::reply::Spoken::new(&submitted.turn_id));
                self.mark(pending.reply_to.as_ref(), super::lifecycle::STARTED)
                    .await;
                self.running = Some(Running {
                    turn_id: submitted.turn_id,
                    requester: pending.routing.sender_id.clone(),
                    reply_to: pending.reply_to,
                    stopped: false,
                    draft: Draft::default(),
                    voice: pending.voice,
                    _typing: typing,
                    speech,
                    watch: super::watchdog::Watch::new(Instant::now()),
                    progress: super::progress::Progress::new(Instant::now()),
                    addressed: pending.addressed,
                    stream: super::native::Stream::default(),
                });
                self.working_status(true).await;
            }
            Err(err) => {
                self.turn_over(&session);
                self.say(err.message());
            }
        }
    }

    /// Marks the session's turn running (restart recovery) and records who
    /// sent it (who pays for its calls under the daily cost cap).
    fn record_start(&self, session: &str, routing: &Routing) {
        let (Some(store), Ok(id)) = (self.shared.store(&self.profile), session.parse()) else {
            return;
        };
        if let Err(err) = store.set_turn_active(id, true) {
            tracing::warn!("turn not recorded as running: {err}");
        }
        let at = Timestamp::now();
        if let Err(err) = store.record_gateway_turn(id, &routing.platform, &routing.sender_id, at) {
            tracing::warn!("turn sender not recorded: {err}");
        }
    }

    /// `[Tue 2026-04-28 13:40:53 CEST]` for a session created with message
    /// timestamps (in the profile's `cron.timezone`, else the host's).
    pub(super) fn timestamp_prefix(&self, session: &str, sent_at: Timestamp) -> Option<String> {
        let store = self.shared.store(&self.profile)?;
        let on = session
            .parse()
            .ok()
            .and_then(|id| store.session_timestamps(id).ok())
            .unwrap_or(false);
        if !on {
            return None;
        }
        let zone = self
            .config()
            .and_then(|s| s.config().cron.timezone.clone())
            .and_then(|name| jiff::tz::TimeZone::get(&name).ok())
            .unwrap_or_else(jiff::tz::TimeZone::system);
        Some(stamp(sent_at, &zone))
    }

    /// The session's link; streaming platforms (and a reply streamed as
    /// audio) get the answer text as it is written.
    fn link(&self, session: &str, routing: &Routing, voice: bool) -> crate::host::Link {
        let link = self.shared.server.link(session);
        let streams = self
            .config()
            .is_some_and(|s| s.config().gateway.streams_on(&routing.platform))
            && self
                .shared
                .adapters
                .descriptor(&self.profile, &routing.platform)
                .is_some_and(|d| d.supports_edit);
        if !streams && !self.streams_audio(voice) {
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
        self.stop_stream().await;
        self.working_status(false).await;
        let Some(mut running) = self.running.take() else {
            return;
        };
        let finished = if running.watch.stalled {
            TurnFinished {
                exit_reason: super::watchdog::STALLED.to_owned(),
                ..finished
            }
        } else {
            finished
        };
        let session = self.session.clone().unwrap_or_default();
        self.turn_over(&session);
        self.mark(
            running.reply_to.as_ref(),
            super::lifecycle::outcome(&finished),
        )
        .await;
        self.offer_title(&session).await;
        if !running.stopped {
            let spoken = match running.speech.take() {
                Some(mut speech) => self.speak_streamed(&mut speech, "", true).await,
                None => false,
            };
            let answer = super::reply::Answer {
                session: &session,
                finished: &finished,
                draft: &running.draft,
                reply_to: running.reply_to.as_ref(),
                voice: running.voice,
                spoken,
                addressed: running.addressed,
            };
            self.deliver_answer(answer).await;
            let succeeded = finished.error.is_none() && finished.exit_reason == "completed";
            self.clean_up_progress(&running.progress, succeeded).await;
        }
        self.shared
            .options
            .hooks
            .emit(
                &self.profile,
                "on_turn_end",
                super::hook_events::turn_end(&session, &finished),
            )
            .await;
        self.flush_mirrors();
        if let Some(Deferred::New { reset, routing }) = self.deferred.take() {
            self.renew(&routing, reset).await;
        }
        self.next().await;
    }

    /// Runs what waits for the turn, in order, until one starts a turn.
    pub(super) async fn next(&mut self) {
        while self.running.is_none() && !self.waiting_for_resume() {
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

    /// Streamed answer text: spoken sentence by sentence when the reply
    /// streams as audio, and shown as an edited draft on streaming
    /// platforms.
    pub(super) async fn delta(&mut self, text: &str) {
        if let Some(mut speech) = self.running.as_mut().and_then(|r| r.speech.take()) {
            self.speak_streamed(&mut speech, text, false).await;
            if let Some(running) = self.running.as_mut() {
                running.speech = Some(speech);
            }
        }
        match self.transport() {
            super::native::Transport::Native => {
                if !self.native_delta(text).await {
                    self.draft(text).await;
                }
            }
            super::native::Transport::Edit => self.draft(text).await,
            super::native::Transport::Off => {}
        }
    }

    /// A draft message sent once enough text arrived, then edited at most
    /// every `edit_interval_ms`.
    async fn draft(&mut self, text: &str) {
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
        let message = Outgoing::text(
            &self.routing.chat_id,
            self.routing.thread_id.as_deref(),
            &format!("{}{}", draft.text, streaming.cursor),
        );
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

/// A send time as the model sees it.
pub(crate) fn stamp(at: Timestamp, zone: &jiff::tz::TimeZone) -> String {
    format!(
        "[{}]",
        at.to_zoned(zone.clone())
            .strftime("%a %Y-%m-%d %H:%M:%S %Z")
    )
}
