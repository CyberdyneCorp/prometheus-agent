//! How a session reports its own start and end and leaves: `session.start`
//! (with its hardening downgrades), events of the session itself (to its
//! trajectory and whoever follows its turns), closing, unloading and
//! ending it.

use std::sync::Arc;

use pmt_agent::turn::EventSink as _;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{SessionEnd, SessionStart};
use pmt_obs::trajectory::writer::Emit;
use pmt_store::{EndReason, Store};

use super::{Live, lifecycle};

impl Live {
    /// Records `session.start` for `session` on `target`'s route, then
    /// one `security.downgrade` per allowed hardening downgrade.
    pub(super) async fn emit_start(
        &self,
        session: &pmt_store::Session,
        target: &pmt_providers::resolve::Target,
    ) {
        self.emit(Event::SessionStart(SessionStart {
            source: session.source.as_str().to_owned(),
            title: session.title.clone(),
            model: target.model.clone(),
            parent_session_id: session.parent_session_id.map(|p| p.to_string()),
            trace_id: None, // stamped from the session's trace
        }))
        .await;
        for event in self.downgrades.events() {
            self.emit(event).await;
        }
        // The gateway fires its own sessions' start through its hooks.
        if self.surface() != "gateway" {
            let fields = lifecycle::session_start(&self.id.to_string(), self.surface());
            self.plugins
                .fire_observer(
                    &self.plugin_call(self.surface()),
                    "on_session_start",
                    fields,
                )
                .await;
        }
    }

    /// Emits `event` outside any turn.
    pub async fn emit(&self, event: Event) {
        let mut emit = Emit::now(event);
        pmt_agent::turn::stamp(&self.recording.trace, &mut emit);
        // The session's own events reach whoever follows its turns too
        // (attached clients, a streamed one-shot run's stdout).
        let followers = self.clients.as_ref().map(|c| &c.events);
        for sink in followers.into_iter().chain(self.recording.stream.as_ref()) {
            let _ = sink.emit(emit.clone()).await;
        }
        let Some(writer) = &self.recording.trajectory else {
            return;
        };
        if let Err(err) = writer.emit(emit).await {
            tracing::debug!(error = %err, "trajectory event dropped");
        }
    }

    /// Ends the session row with `reason`, records `session.end` and
    /// closes the trajectory.
    pub async fn close(self, store: &Store, reason: EndReason) {
        self.end(store, reason).await;
        // Webhooks of the session's last events go out before the process
        // that ran it may exit.
        crate::hooks_host::outbound::flush(crate::hooks_host::outbound::FLUSH).await;
        // Everything else the session holds (its agent, tools, delegation)
        // may reach the writer through its event sinks: it is dropped
        // first so the writer can be finished.
        let trajectory = self.recording.trajectory.clone();
        drop(self);
        if let Some(writer) = trajectory.and_then(|t| Arc::try_unwrap(t).ok())
            && let Err(err) = writer.finish()
        {
            tracing::warn!(error = %err, "could not close the trajectory");
        }
    }

    /// Unloads the session without ending it (the messaging gateway's
    /// agent cache): the record stays open and its next turn rebuilds the
    /// agent from the store with the same frozen prompt. Built-in memory
    /// is written to the session's own profile store at every call, so
    /// nothing is left to flush.
    pub async fn unload(self) {
        crate::memory_provider::forget(&self.id.to_string());
        if let Some(browser) = &self.place.browser() {
            browser.close_session(&self.id.to_string()).await;
        }
        // Everything else the session holds (its agent, tools, delegation)
        // may reach the writer through its event sinks: it is dropped
        // first so the writer can be finished.
        let trajectory = self.recording.trajectory.clone();
        drop(self);
        if let Some(writer) = trajectory.and_then(|t| Arc::try_unwrap(t).ok())
            && let Err(err) = writer.finish()
        {
            tracing::warn!(error = %err, "could not close the trajectory");
        }
    }

    /// Ends the session row with `reason` and records `session.end`,
    /// leaving the trajectory file open: for a session something else
    /// still holds when the process stops (prefer [`Live::close`]).
    pub async fn end(&self, store: &Store, reason: EndReason) {
        if let Some(delegation) = &self.delegation {
            delegation.close();
        }
        pmt_tools::code_exec::kernel::close_session(&self.id.to_string());
        crate::memory_provider::close(&self.id.to_string()).await;
        self.emit(Event::SessionEnd(SessionEnd {
            reason: reason.as_str().to_owned(),
        }))
        .await;
        if let Err(err) = store.end_session(self.id, reason) {
            tracing::warn!(error = %err, session = %self.id, "could not end the session");
        }
        // The gateway fires its own sessions' end through its hooks.
        if self.surface() != "gateway" {
            let fields = serde_json::json!({
                "session_id": self.id.to_string(),
                "reason": reason.as_str(),
                "platform": self.surface(),
            });
            if let serde_json::Value::Object(fields) = fields {
                self.plugins
                    .fire_observer(&self.plugin_call(self.surface()), "on_session_end", fields)
                    .await;
            }
        }
        if let Some(browser) = &self.place.browser() {
            browser.close_session(&self.id.to_string()).await;
        }
        // Logged, so in English.
        let english = pmt_i18n::Strings::english(pmt_i18n::Surface::Core);
        if let Some(text) = self.sync_back(&*english).await {
            tracing::info!(session = %self.id, "{text}");
        }
        self.place.release();
    }
}
