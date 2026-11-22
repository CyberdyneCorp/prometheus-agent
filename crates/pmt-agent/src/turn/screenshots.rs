//! The screenshot context budget (spec: computer-use, Screenshot context
//! budget).
//!
//! Images a tool returned (computer-use captures, browser screenshots)
//! travel with their tool row, as pixels after the run of tool results
//! they belong to. They are kept while the request stays within the
//! provider's per-request image limit — [`MANY`] images when every image
//! is at most [`SMALL_SIDE`] px per side, otherwise [`FEW`] — and
//! [`MAX_BYTES`] of image data. Past that, the oldest screenshots are
//! replaced by [`RETIRED`] in whole batches (half the image limit, or half
//! of [`FEW`] for the byte budget), so an
//! earlier message is rewritten only once per batch; user-attached images
//! count toward the limits but are never removed. The newest retired
//! screenshot row is recorded in the store, so a screenshot once retired
//! stays retired for the rest of the session (a compaction that drops rows
//! never brings one back). Below the limits nothing is rewritten and the
//! cached prompt prefix holds.

use pmt_core::id::{MessageId, SessionId};
use pmt_store::{Message, Role, Store};

use super::transcript::{self, ImageRef};

/// The placeholder of a retired screenshot.
pub const RETIRED: &str = "[screenshot removed to save context]";
/// The image limit of a request whose images are all small.
pub const MANY: usize = 100;
/// The image limit otherwise.
pub const FEW: usize = 20;
/// The longest side of a "small" image.
pub const SMALL_SIDE: u32 = 2_000;
/// The image data a request may carry.
pub const MAX_BYTES: u64 = 24 * 1024 * 1024;

/// The store preference holding a session's retirement mark.
fn pref(session: SessionId) -> String {
    format!("screenshots.retired.{session}")
}

/// One image of the request, in transcript order.
struct Item {
    row: MessageId,
    screenshot: bool,
    side: u32,
    bytes: u64,
}

impl Item {
    fn of(row: &Message, image: &ImageRef) -> Self {
        let (width, height) = image.pixels.unwrap_or((u32::MAX, u32::MAX));
        Self {
            row: row.id,
            screenshot: row.role == Role::Tool,
            side: width.max(height),
            bytes: image.bytes.unwrap_or(0),
        }
    }
}

/// Which screenshot rows are retired: those at or before the mark.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Retired(Option<MessageId>);

impl Retired {
    /// Whether the screenshots of `row` are retired.
    pub(super) fn covers(self, row: MessageId) -> bool {
        self.0.is_some_and(|mark| row <= mark)
    }
}

/// The retirement mark for a request built from `rows`, persisted when it
/// moves (images carrying a description are text, not pixels).
pub(super) fn retired(store: &Store, session: SessionId, rows: &[Message]) -> Retired {
    let stored = store
        .pref(&pref(session))
        .ok()
        .flatten()
        .and_then(|text| MessageId::parse(&text).ok());
    let items: Vec<Item> = rows
        .iter()
        .flat_map(|row| {
            transcript::images_of(row)
                .into_iter()
                .filter(|image| image.description.is_none())
                .map(move |image| Item::of(row, &image))
        })
        .collect();
    let mark = mark(&items, stored);
    if mark != stored
        && let Some(mark) = mark
        && let Err(err) = store.set_pref(&pref(session), &mark.to_string())
    {
        tracing::warn!(error = %err, "could not record the retired screenshots");
    }
    Retired(mark)
}

/// The newest screenshot row to retire so the kept images fit, starting
/// from `stored`.
fn mark(items: &[Item], stored: Option<MessageId>) -> Option<MessageId> {
    let retired = |mark: Option<MessageId>, item: &Item| {
        item.screenshot && mark.is_some_and(|m| item.row <= m)
    };
    let mut mark = stored;
    loop {
        let kept: Vec<&Item> = items.iter().filter(|item| !retired(mark, item)).collect();
        let limit = if kept.iter().all(|item| item.side <= SMALL_SIDE) {
            MANY
        } else {
            FEW
        };
        let bytes: u64 = kept.iter().map(|item| item.bytes).sum();
        if kept.len() <= limit && bytes <= MAX_BYTES {
            return mark;
        }
        // Too many images retire half the limit at once; too many bytes
        // (with few enough images) half the smaller limit.
        let batch = if kept.len() > limit {
            limit / 2
        } else {
            FEW / 2
        };
        let next = kept
            .iter()
            .filter(|item| item.screenshot)
            .take(batch)
            .last()?;
        mark = Some(next.row);
    }
}

#[cfg(test)]
mod tests {
    use pmt_core::id::MessageId;

    use super::{Item, MAX_BYTES, mark};

    fn shots(count: usize, side: u32, bytes: u64) -> Vec<Item> {
        (0..count)
            .map(|_| Item {
                row: MessageId::new(),
                screenshot: true,
                side,
                bytes,
            })
            .collect()
    }

    #[test]
    fn below_the_limit_nothing_is_retired() {
        assert_eq!(mark(&shots(30, 1456, 400_000), None), None);
        assert_eq!(mark(&shots(100, 1456, 100_000), None), None);
    }

    #[test]
    fn over_the_limit_the_oldest_retire_in_a_batch() {
        let items = shots(101, 1456, 100_000);
        assert_eq!(
            mark(&items, None),
            Some(items[49].row),
            "half the limit at once"
        );
        let large = shots(21, 2400, 100_000);
        assert_eq!(mark(&large, None), Some(large[9].row));
        let heavy = shots(30, 1456, MAX_BYTES / 25);
        assert_eq!(
            mark(&heavy, None),
            Some(heavy[9].row),
            "the byte budget retires too"
        );
    }

    #[test]
    fn user_images_are_never_retired_and_a_mark_never_moves_back() {
        let mut items = shots(24, 2400, 1_000);
        for item in items.iter_mut().step_by(2) {
            item.screenshot = false;
        }
        let first = mark(&items, None).unwrap();
        assert!(items.iter().find(|i| i.row == first).unwrap().screenshot);
        let few = shots(3, 100, 1);
        let kept = Some(few[1].row);
        assert_eq!(
            mark(&few, kept),
            kept,
            "a retired screenshot is not restored"
        );
    }
}
