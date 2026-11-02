//! Plain input (spec: messaging-gateway, Busy input policy from
//! configuration only): text is debounced into one input, then starts a
//! turn, or while one runs is queued, steers it or interrupts it per
//! `busy.input_mode` (overridable per platform); media follow-ups join the
//! pending input without interrupting; at most one busy acknowledgement
//! per conversation per 30 seconds.

use std::fmt::Write as _;
use std::time::Duration;

use pmt_config::schema::{BusyInputMode, EffectiveBusy};
use tokio::time::Instant;

use super::{Lane, Origin, Pending};
use crate::messaging::adapter::{Inbound, MediaKind};
use crate::messaging::key::Routing;

/// Least time between two busy acknowledgements of a conversation.
const ACK_EVERY: Duration = Duration::from_secs(30);

/// Text gathered before it becomes one input.
#[derive(Debug)]
pub(crate) struct Buffer {
    pending: Pending,
    first: Instant,
    last: Instant,
    window: Duration,
    cap: Duration,
}

impl Buffer {
    /// When the gathered text becomes an input.
    pub(crate) fn due(&self) -> Instant {
        (self.last + self.window).min(self.first + self.cap)
    }
}

/// The text and images of an event; other attachments become a note.
fn content(inbound: &Inbound) -> (String, Vec<String>) {
    let mut text = inbound.text.trim().to_owned();
    let mut images = Vec::new();
    for attachment in &inbound.attachments {
        if attachment.kind == MediaKind::Image {
            images.push(attachment.location.clone());
        } else {
            if !text.is_empty() {
                text.push('\n');
            }
            let _ = write!(
                text,
                "[attachment: {:?} {}]",
                attachment.kind, attachment.location
            );
        }
    }
    (text, images)
}

impl Lane {
    /// The busy settings of the conversation's platform now.
    pub(super) fn busy(&self) -> EffectiveBusy {
        let busy = self.config().map_or(
            EffectiveBusy {
                input_mode: BusyInputMode::Queue,
                debounce_ms: 350,
                debounce_cap_ms: 1000,
                ack: true,
                max_queued: 16,
            },
            |scope| {
                let config = scope.config();
                config
                    .gateway
                    .busy_for(&self.routing.platform, config.busy.input_mode)
            },
        );
        self.with_policy_busy(busy)
    }

    /// Plain text or media from the conversation.
    pub(super) async fn text(&mut self, routing: Routing, inbound: &Inbound) {
        let (text, images) = content(inbound);
        if text.is_empty() && images.is_empty() {
            return;
        }
        let pending = Pending {
            origin: Origin::Human,
            text,
            images,
            routing,
            reply_to: Some(inbound.message_id.clone()).filter(|id| !id.is_empty()),
            voice: self.voice_input,
        };
        if self.running.is_some() && pending.text.is_empty() {
            // Media follow-ups never interrupt or steer.
            self.queue_human(pending);
            return;
        }
        let (window, cap) = self.gather_window(&inbound.text);
        let now = Instant::now();
        match &mut self.buffer {
            Some(buffer) => {
                join(&mut buffer.pending, pending);
                buffer.last = now;
                buffer.window = window;
            }
            None => {
                self.buffer = Some(Buffer {
                    pending,
                    first: now,
                    last: now,
                    window,
                    cap,
                });
            }
        }
        if window.is_zero() {
            self.flush().await;
        }
    }

    /// How long to wait for more text after `text`, and the longest an
    /// input is held: the platform's batching when set (a longer wait
    /// after a part that filled the platform's limit, likely split by the
    /// client), else the busy debounce.
    fn gather_window(&self, text: &str) -> (Duration, Duration) {
        let busy = self.busy();
        let batching = self.config().and_then(|scope| {
            scope
                .config()
                .gateway
                .platform(&self.routing.platform)
                .batching
        });
        let Some(batching) = batching else {
            return (
                Duration::from_millis(busy.debounce_ms),
                Duration::from_millis(busy.debounce_cap_ms),
            );
        };
        let split = self
            .shared
            .adapters
            .descriptor(&self.profile, &self.routing.platform)
            .is_some_and(|d| d.length_unit.len(text) * 10 >= d.max_message_length * 9);
        let window = if split {
            batching.split_delay_ms
        } else {
            batching.delay_ms
        };
        let cap = busy.debounce_cap_ms.max(batching.split_delay_ms);
        (Duration::from_millis(window), Duration::from_millis(cap))
    }

    /// The gathered text becomes one input.
    pub(super) async fn flush(&mut self) {
        let Some(buffer) = self.buffer.take() else {
            return;
        };
        if self.running.is_some() {
            self.while_busy(buffer.pending).await;
        } else {
            self.start_turn(buffer.pending).await;
        }
    }

    async fn while_busy(&mut self, pending: Pending) {
        let busy = self.busy();
        match busy.input_mode {
            BusyInputMode::Queue => {
                self.queue_human(pending);
                self.ack(
                    &busy,
                    "Still working on your previous message; this one is next.",
                );
            }
            BusyInputMode::Interrupt => {
                self.queue_human(pending);
                self.interrupt().await;
                self.ack(
                    &busy,
                    "Stopping the current task to handle your new message.",
                );
            }
            BusyInputMode::Steer => {
                let steered = pending.images.is_empty() && self.steer(&pending.text).await;
                if steered {
                    self.ack(
                        &busy,
                        "Got it; the current task will take that into account.",
                    );
                } else {
                    self.queue_human(pending);
                }
            }
        }
    }

    async fn steer(&self, text: &str) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        match self.shared.agents.steer(&self.profile, session, text).await {
            Ok(steered) => steered,
            Err(err) => {
                tracing::warn!("steering failed: {err}");
                false
            }
        }
    }

    /// Stops the running turn at its next safe point.
    pub(super) async fn interrupt(&self) {
        if let Some(session) = &self.session
            && let Err(err) = self.shared.agents.interrupt(&self.profile, session).await
        {
            tracing::warn!("interrupt failed: {err}");
        }
    }

    /// Adds a follow-up to the human input waiting for the turn (merged
    /// with the last one unless a `/queue` entry or command came between).
    pub(super) fn queue_human(&mut self, pending: Pending) {
        match self.pending.back_mut() {
            Some(last) if last.origin == Origin::Human => join(last, pending),
            _ => self.pending.push_back(pending),
        }
    }

    fn ack(&mut self, busy: &EffectiveBusy, text: &str) {
        let now = Instant::now();
        if !busy.ack
            || self
                .last_ack
                .is_some_and(|at| now.duration_since(at) < ACK_EVERY)
        {
            return;
        }
        self.last_ack = Some(now);
        self.say(text);
    }
}

fn join(into: &mut Pending, more: Pending) {
    if !more.text.is_empty() {
        if !into.text.is_empty() {
            into.text.push('\n');
        }
        into.text.push_str(&more.text);
    }
    into.images.extend(more.images);
    into.voice |= more.voice;
    into.routing = more.routing;
    if more.reply_to.is_some() {
        into.reply_to = more.reply_to;
    }
}
