//! Plain input (spec: messaging-gateway, Busy input policy from
//! configuration only): text is debounced into one input, then starts a
//! turn, or while one runs is queued, steers it or interrupts it per
//! `busy.input_mode` (overridable per platform); media follow-ups join the
//! pending input without interrupting; at most one busy acknowledgement
//! per conversation per 30 seconds.

use std::fmt::Write as _;
use std::time::Duration;

use pmt_config::schema::{Batching, BusyInputMode, EffectiveBusy};
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

/// The text, images and other files of an event; a file also becomes a
/// note naming it.
fn content(inbound: &Inbound) -> (String, Vec<String>, Vec<String>) {
    let mut text = inbound.text.trim().to_owned();
    let mut images = Vec::new();
    let mut files = Vec::new();
    for attachment in &inbound.attachments {
        if attachment.kind == MediaKind::Image {
            images.push(attachment.location.clone());
        } else {
            if std::path::Path::new(&attachment.location).is_absolute() {
                files.push(attachment.location.clone());
            }
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
    (text, images, files)
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
        let (text, images, files) = content(inbound);
        if text.is_empty() && images.is_empty() {
            return;
        }
        let pending = Pending {
            origin: Origin::Human,
            text,
            images,
            files,
            routing,
            reply_to: Some(inbound.message_id.clone()).filter(|id| !id.is_empty()),
            voice: self.voice_input,
            billed: std::mem::take(&mut self.voice_billed),
            sent_at: inbound.timestamp,
            addressed: !inbound.addressing.not_addressed,
        };
        if self.running.is_some() && pending.text.is_empty() {
            // Media follow-ups never interrupt or steer.
            self.queue_human(pending);
            return;
        }
        let (window, cap) = if inbound.is_bot {
            self.burst_window()
        } else {
            self.gather_window(&inbound.text)
        };
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
        let platform = &self.routing.platform;
        // WhatsApp clients split rapid messages: batched by default.
        let batching = self.config().and_then(|scope| {
            scope
                .config()
                .gateway
                .platform(platform)
                .batching
                .or_else(|| (platform == "whatsapp").then(Batching::default))
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

    /// How long a bot's message waits for its follow-up chunks
    /// (`gateway.bots.burst_window_ms`), so a multipart message is one
    /// turn.
    fn burst_window(&self) -> (Duration, Duration) {
        let window = self.config().map_or(1500, |s| {
            s.config()
                .gateway
                .bots
                .for_platform(&self.routing.platform)
                .burst_window_ms
        });
        let window = Duration::from_millis(window);
        (window, window * 4)
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
                self.ack(&busy, "gateway.busy_queued");
            }
            BusyInputMode::Interrupt => {
                self.queue_human(pending);
                self.interrupt().await;
                self.ack(&busy, "gateway.busy_interrupting");
            }
            BusyInputMode::Steer => {
                let steered = pending.images.is_empty() && self.steer(&pending.text).await;
                if steered {
                    self.ack(&busy, "gateway.busy_steered");
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

    /// Acknowledges input that arrived while busy with catalog text `key`.
    fn ack(&mut self, busy: &EffectiveBusy, key: &str) {
        let now = Instant::now();
        if !busy.ack
            || self
                .last_ack
                .is_some_and(|at| now.duration_since(at) < ACK_EVERY)
        {
            return;
        }
        self.last_ack = Some(now);
        let detail = self
            .running
            .as_ref()
            .filter(|_| self.display().busy_ack_detail)
            .map(|r| r.progress.activity(now));
        let strings = self.strings();
        let text = strings.get(key);
        match detail {
            Some(detail) => {
                let detail = detail.render(&strings);
                self.say(&pmt_i18n::t!(
                    strings,
                    "gateway.busy_ack_detail",
                    text = text,
                    detail = detail
                ));
            }
            None => self.say(text),
        }
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
    into.files.extend(more.files);
    into.voice |= more.voice;
    into.billed.extend(more.billed);
    into.addressed |= more.addressed;
    into.routing = more.routing;
    if more.reply_to.is_some() {
        into.reply_to = more.reply_to;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_downloaded_document_is_a_file_and_a_note() {
        let mut inbound = Inbound::text(
            crate::messaging::key::ChatType::Dm,
            "chat",
            "user",
            "see attached",
        );
        inbound
            .attachments
            .push(crate::messaging::adapter::Attachment::new(
                MediaKind::Document,
                "/cache/media/abc.pdf",
            ));
        let (text, images, files) = content(&inbound);
        assert!(images.is_empty());
        assert_eq!(files, vec!["/cache/media/abc.pdf".to_owned()]);
        assert!(
            text.contains("[attachment: Document /cache/media/abc.pdf]"),
            "{text}"
        );
    }
}
