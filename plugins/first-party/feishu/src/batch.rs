//! Inbound batching: people type a thought as several quick messages, and
//! send photos one by one. Text messages from one sender in one chat that
//! follow each other within `batch_delay_ms` (600) become one inbound
//! message, flushed early at `batch_max_messages` (8) or
//! `batch_max_chars` (4000); messages carrying files wait
//! `media_batch_delay_ms` (800) for the rest of the burst (a caption sent
//! after a photo joins it). Delays run on the poll clock. A delay of 0
//! turns that batching off.

use adapter_kit::clock;
use adapter_kit::message::Inbound;
use serde_json::Value;

/// The batching settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    text_ms: u64,
    media_ms: u64,
    max_messages: usize,
    max_chars: usize,
}

impl Limits {
    /// The settings in `options`.
    #[must_use]
    pub fn new(options: &Value) -> Self {
        let number = |key: &str, default: u64| options[key].as_u64().unwrap_or(default);
        Self {
            text_ms: number("batch_delay_ms", 600),
            media_ms: number("media_batch_delay_ms", 800),
            max_messages: usize::try_from(number("batch_max_messages", 8))
                .unwrap_or(8)
                .max(1),
            max_chars: usize::try_from(number("batch_max_chars", 4000))
                .unwrap_or(4000)
                .max(1),
        }
    }
}

struct Pending {
    message: Inbound,
    count: usize,
    media: bool,
    last_ms: u64,
}

/// The open batches.
pub struct Batches {
    limits: Limits,
    open: Vec<Pending>,
}

fn same_batch(a: &Inbound, b: &Inbound) -> bool {
    a.chat == b.chat && a.sender == b.sender && a.thread == b.thread
}

impl Batches {
    /// No open batch.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            open: Vec::new(),
        }
    }

    /// Adds `message`; what is ready to go now.
    pub fn push(&mut self, message: Inbound) -> Vec<Inbound> {
        let media = !message.attachments.is_empty();
        let delay = if media {
            self.limits.media_ms
        } else {
            self.limits.text_ms
        };
        // A pressed button or a command is never merged; a caption that
        // follows a file joins the file's batch even with text batching
        // off.
        let joins = self.open.iter().any(|p| same_batch(&p.message, &message));
        if (delay == 0 && !joins) || message.callback.is_some() || message.text.starts_with('/') {
            return vec![message];
        }
        let now = clock::now_ms();
        if let Some(open) = self
            .open
            .iter_mut()
            .find(|p| same_batch(&p.message, &message))
        {
            if !open.message.text.is_empty() && !message.text.is_empty() {
                open.message.text.push('\n');
            }
            open.message.text.push_str(&message.text);
            open.message.attachments.extend(message.attachments);
            open.message.id = message.id;
            open.message.mentioned |= message.mentioned;
            open.count += 1;
            open.media |= media;
            open.last_ms = now;
        } else {
            self.open.push(Pending {
                message,
                count: 1,
                media,
                last_ms: now,
            });
        }
        let limits = self.limits;
        self.take(|p| {
            p.count >= limits.max_messages || p.message.text.chars().count() >= limits.max_chars
        })
    }

    /// The batches whose burst is over.
    pub fn due(&mut self) -> Vec<Inbound> {
        let (now, limits) = (clock::now_ms(), self.limits);
        self.take(|p| {
            let wait = if p.media {
                limits.media_ms
            } else {
                limits.text_ms
            };
            now.saturating_sub(p.last_ms) >= wait
        })
    }

    fn take(&mut self, ready: impl Fn(&Pending) -> bool) -> Vec<Inbound> {
        let (done, open): (Vec<Pending>, Vec<Pending>) =
            std::mem::take(&mut self.open).into_iter().partition(ready);
        self.open = open;
        done.into_iter().map(|p| p.message).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_burst_becomes_one_message_and_the_cap_flushes_early() {
        let mut batches = Batches::new(Limits::new(&json!({"batch_max_messages": 3})));
        let text = |id: &str, t: &str| Inbound::text(id, "oc_1", "dm", "ou_a", t);
        assert!(batches.push(text("1", "one")).is_empty());
        assert!(batches.push(text("2", "two")).is_empty());
        let flushed = batches.push(text("3", "three"));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].text, "one\ntwo\nthree");
        assert_eq!(flushed[0].id, "3");
        let mut off = Batches::new(Limits::new(&json!({"batch_delay_ms": 0})));
        assert_eq!(off.push(text("4", "now")).len(), 1);
        let mut photo = text("6", "");
        photo.attachments.push(adapter_kit::message::InFile::bytes(
            "image",
            "a.png",
            vec![1],
        ));
        assert!(off.push(photo).is_empty(), "a file waits for its burst");
        assert!(
            off.push(text("7", "caption")).is_empty(),
            "its caption joins it"
        );
        assert_eq!(off.open[0].message.text, "caption");
        assert_eq!(batches.push(text("5", "/stop")).len(), 1);
    }
}
