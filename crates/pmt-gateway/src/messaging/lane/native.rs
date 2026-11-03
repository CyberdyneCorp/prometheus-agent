//! Native platform streaming, task cards and the working status line
//! (spec: messaging-gateway, Native platform streaming and task cards).
//!
//! - `gateway.streaming.transport`: `auto` (the default) and `native` use
//!   the platform's own stream where the descriptor declares
//!   `native_stream`, `edit` (and `draft`, which has no platform API yet)
//!   edit one message, `off` streams nothing. A stream that fails
//!   mid-turn is stopped and the same message is edited from then on with
//!   the whole text so far, so the answer appears exactly once.
//! - With `task_cards`, tool calls show as native task cards in the stream
//!   (updated in place as they finish) instead of progress bubbles.
//! - With `status_line`, the chat shows a working status while the turn
//!   runs, if `typing_indicator` is on for the platform.

use pmt_config::schema::StreamTransport;

use super::Lane;
use crate::messaging::adapter::{Outgoing, TaskCard, TaskStatus};

/// What the working status says.
pub(crate) const THINKING: &str = "is thinking…";

/// How a turn's answer streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Transport {
    Off,
    Edit,
    Native,
}

/// The native stream of the running turn.
#[derive(Debug, Default)]
pub(crate) struct Stream {
    /// The message it streams into.
    pub(super) message_id: Option<String>,
    /// It failed: the turn goes on by editing that message.
    pub(super) failed: bool,
}

impl Lane {
    /// The transport of the conversation's platform now.
    pub(super) fn transport(&self) -> Transport {
        let Some(scope) = self.config() else {
            return Transport::Off;
        };
        let gateway = &scope.config().gateway;
        if !gateway.streams_on(&self.routing.platform) {
            return Transport::Off;
        }
        let descriptor = self
            .shared
            .adapters
            .descriptor(&self.profile, &self.routing.platform);
        let native = descriptor.as_ref().is_some_and(|d| d.native_stream);
        let edits = descriptor.as_ref().is_some_and(|d| d.supports_edit);
        match gateway.streaming.transport {
            StreamTransport::Off => Transport::Off,
            StreamTransport::Auto | StreamTransport::Native if native => Transport::Native,
            _ if edits => Transport::Edit,
            _ => Transport::Off,
        }
    }

    /// The running turn's stream message, started on first use; `None`
    /// once the stream failed (or never started).
    async fn stream_message(&mut self) -> Option<String> {
        let running = self.running.as_ref()?;
        if running.stream.failed {
            return None;
        }
        if let Some(id) = &running.stream.message_id {
            return Some(id.clone());
        }
        let adapter = self
            .shared
            .adapters
            .adapter(&self.profile, &self.routing.platform)?;
        let mut message =
            Outgoing::text(&self.routing.chat_id, self.routing.thread_id.as_deref(), "");
        message.reply_to.clone_from(&running.reply_to);
        let started = adapter.stream_start(&message).await;
        let running = self.running.as_mut()?;
        match started {
            Ok(id) => {
                running.stream.message_id = Some(id.clone());
                // The final answer edits the streamed message.
                running.draft.message_id = Some(id.clone());
                Some(id)
            }
            Err(err) => {
                tracing::info!("no native stream, editing instead: {}", err.describe());
                running.stream.failed = true;
                None
            }
        }
    }

    /// Streamed text over the native stream; `false` when the stream is
    /// not (or no longer) usable and the caller edits a draft instead.
    pub(super) async fn native_delta(&mut self, text: &str) -> bool {
        let Some(id) = self.stream_message().await else {
            return false;
        };
        let Some(adapter) = self
            .shared
            .adapters
            .adapter(&self.profile, &self.routing.platform)
        else {
            return false;
        };
        if let Some(running) = self.running.as_mut() {
            running.draft.text.push_str(text);
        }
        match adapter
            .stream_append(&self.routing.chat_id, &id, text)
            .await
        {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(
                    "the native stream failed mid-turn, editing instead: {}",
                    err.describe()
                );
                self.fall_back_to_edit(&id).await;
                true
            }
        }
    }

    /// Stops the failed stream and shows the whole text so far by editing
    /// its message; later deltas edit it too.
    async fn fall_back_to_edit(&mut self, id: &str) {
        let Some(running) = self.running.as_mut() else {
            return;
        };
        running.stream.failed = true;
        let text = running.draft.text.clone();
        let adapters = &self.shared.adapters;
        if let Some(adapter) = adapters.adapter(&self.profile, &self.routing.platform) {
            let _ = adapter.stream_stop(&self.routing.chat_id, id).await;
        }
        let message = Outgoing::text(
            &self.routing.chat_id,
            self.routing.thread_id.as_deref(),
            &text,
        );
        let edited = adapters
            .edit_direct(&self.profile, &self.routing.platform, id, &message)
            .await;
        if let Some(running) = self.running.as_mut() {
            running.draft.last_edit = Some(tokio::time::Instant::now());
            if edited.is_err() {
                // The final answer is then sent as a message of its own.
                running.draft.failed = true;
                running.draft.message_id = None;
            }
        }
    }

    /// Ends the running turn's stream before its final answer edits it.
    pub(super) async fn stop_stream(&mut self) {
        let Some(id) = self
            .running
            .as_ref()
            .filter(|r| !r.stream.failed)
            .and_then(|r| r.stream.message_id.clone())
        else {
            return;
        };
        if let Some(adapter) = self
            .shared
            .adapters
            .adapter(&self.profile, &self.routing.platform)
            && let Err(err) = adapter.stream_stop(&self.routing.chat_id, &id).await
        {
            tracing::debug!("stream not stopped: {}", err.describe());
        }
    }

    /// A tool call or result as a native task card; `false` when the
    /// platform has none (the caller shows a progress line instead).
    pub(super) async fn task_card(&mut self, id: &str, title: &str, status: TaskStatus) -> bool {
        let cards = self
            .shared
            .adapters
            .descriptor(&self.profile, &self.routing.platform)
            .is_some_and(|d| d.task_cards);
        if !cards {
            return false;
        }
        let Some(stream) = self.stream_message().await else {
            return false;
        };
        let Some(adapter) = self
            .shared
            .adapters
            .adapter(&self.profile, &self.routing.platform)
        else {
            return false;
        };
        let card = TaskCard {
            id: id.to_owned(),
            title: title.to_owned(),
            status,
        };
        adapter
            .stream_task(&self.routing.chat_id, &stream, &card)
            .await
            .is_ok()
    }

    /// Shows (or clears) the working status where the platform has one
    /// and the typing indicator is on.
    pub(super) async fn working_status(&self, on: bool) {
        let declared = self
            .shared
            .adapters
            .descriptor(&self.profile, &self.routing.platform)
            .is_some_and(|d| d.status_line);
        let typing = self
            .config()
            .is_some_and(|s| s.config().gateway.platform(&self.routing.platform).typing());
        if !declared || !typing {
            return;
        }
        let Some(adapter) = self
            .shared
            .adapters
            .adapter(&self.profile, &self.routing.platform)
        else {
            return;
        };
        let thread = self
            .routing
            .thread_id
            .as_deref()
            .or_else(|| self.running.as_ref().and_then(|r| r.reply_to.as_deref()));
        let status = on.then_some(THINKING);
        if let Err(err) = adapter
            .set_status(&self.routing.chat_id, thread, status)
            .await
        {
            tracing::debug!("working status not shown: {}", err.describe());
        }
    }
}
