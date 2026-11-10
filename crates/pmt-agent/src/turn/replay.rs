//! Image replay in requests (spec: agent-turn-loop, Image budgets and
//! replay; web-and-media-tools, Image embed budget, per-image cap and
//! replay).
//!
//! Every request is rebuilt from the stored transcript, so the same rule
//! serves a running session and one resumed in a new process: the user
//! images of the current turn's message are always sent; of the older
//! ones only the newest `vision.replay_recent_images` stay, retired in
//! batches of that size (between N and 2N-1 stay), so an earlier message
//! is rewritten at most once per batch and the cached prefix breaks only
//! then. A retired image becomes `[image]`. Each kept image is re-encoded
//! deterministically within `vision.embed_target_bytes`, so it is
//! byte-identical on every send.

use std::collections::HashMap;

use pmt_core::id::MessageId;
use pmt_providers::request::Part;
use pmt_store::{Message, Store};
use pmt_tools::vision::budget::clamp_target;
use pmt_tools::vision::routing::{Inline, inline};

use super::transcript;

/// The placeholder of an image no longer replayed.
pub(super) const RETIRED: &str = "[image]";

/// The index of the first image still replayed among `total`, keeping
/// the newest `keep`, retired in batches of `keep`.
#[must_use]
pub(super) fn cutoff(total: usize, keep: usize) -> usize {
    if keep == 0 {
        return total;
    }
    if total <= keep {
        return 0;
    }
    (total - keep) / keep * keep
}

/// Which stored images a request carries.
pub(super) struct Replay {
    /// Index of each image row's first image.
    first: HashMap<MessageId, usize>,
    cutoff: usize,
    /// The current turn's user row: always sent in full.
    current: Option<MessageId>,
    target: u64,
}

impl Replay {
    /// The replay of `rows` keeping `keep` older images.
    pub(super) fn of(rows: &[Message], keep: u32, current: Option<MessageId>, target: u64) -> Self {
        let mut first = HashMap::new();
        let mut total = 0;
        for row in rows {
            let count = transcript::images_of(row).len();
            if count > 0 && Some(row.id) != current {
                first.insert(row.id, total);
                total += count;
            }
        }
        Self {
            first,
            cutoff: cutoff(total, usize::try_from(keep).unwrap_or(usize::MAX)),
            current,
            target: clamp_target(target),
        }
    }

    /// The parts of `row`'s images: pixels within the embed budget,
    /// descriptions, notes, or the retired placeholder.
    pub(super) fn parts(&self, store: &Store, row: &Message) -> Vec<Part> {
        let start = (Some(row.id) != self.current)
            .then(|| self.first.get(&row.id).copied())
            .flatten();
        transcript::images_of(row)
            .into_iter()
            .enumerate()
            .map(|(k, image)| {
                if let Some(description) = &image.description {
                    return Part::Text {
                        text: format!("[image {}: {description}]", image.name),
                    };
                }
                if start.is_some_and(|s| s + k < self.cutoff) {
                    return Part::Text {
                        text: RETIRED.to_owned(),
                    };
                }
                self.pixels(store, &image)
            })
            .collect()
    }

    fn pixels(&self, store: &Store, image: &transcript::ImageRef) -> Part {
        let bytes = pmt_store::BlobRef::parse(&image.blob)
            .ok_or_else(|| "bad blob key".to_owned())
            .and_then(|blob| store.get_blob(&blob).map_err(|e| e.to_string()));
        let data = bytes.and_then(pmt_tools::vision::ImageData::sniff);
        let fitted = data.map(|data| match inline(data, &image.name) {
            Inline::Image(image) => image.fitted(self.target).map(Inline::Image),
            note @ Inline::Note(_) => Ok(note),
        });
        match fitted {
            Ok(Ok(Inline::Image(image))) => Part::Image {
                data: image.base64(),
                media_type: image.mime,
                is_url: false,
            },
            Ok(Ok(Inline::Note(note))) => Part::Text { text: note },
            Ok(Err(why)) | Err(why) => Part::Text {
                text: format!("[image {} is unavailable: {why}]", image.name),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::cutoff;

    #[test]
    fn older_images_retire_in_batches() {
        assert_eq!(cutoff(2, 3), 0);
        assert_eq!(cutoff(3, 3), 0);
        assert_eq!(cutoff(5, 3), 0, "up to 2N-1 stay");
        assert_eq!(cutoff(6, 3), 3, "a batch of N retires at once");
        assert_eq!(cutoff(8, 3), 3);
        assert_eq!(cutoff(9, 3), 6);
        assert_eq!(cutoff(4, 0), 4, "0 disables replay");
    }
}
