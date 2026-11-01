//! Prompt-cache break detection (spec: agent-turn-loop, Cache-hit
//! reporting and break detection; observability).
//!
//! Each request's prefix (system blocks, tools, messages) is hashed in its
//! canonical form (cache markers excluded) and compared with the previous
//! request of the session: everything the previous request sent must come
//! back unchanged. A change without a committed compaction or a recorded
//! `prompt.rebuilt` (both reset the comparison) is a bug that multiplies
//! the user's cost, reported as a WARN `cache.unexpected_break` event
//! naming the first differing message, and logged at ERROR in debug and
//! test builds so it cannot pass unnoticed there. The summary handoff's
//! toolless request is not compared. This turn's recall block is not part
//! of the canonical form: it rides on the wire for one turn only
//! ([`super::recall`]).

use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::CacheBreak;
use pmt_providers::request::Request;
use serde::Serialize;

use super::run::Turn;

/// The canonical digests of one request's prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Prefix {
    /// System blocks and tools.
    head: String,
    /// One digest per message.
    messages: Vec<String>,
}

impl Prefix {
    pub(super) fn of(request: &Request) -> Self {
        Self {
            head: digest(&(&request.system, &request.tools)),
            messages: request.messages.iter().map(digest).collect(),
        }
    }

    /// The hash of the first `len` messages under the head.
    fn hash(&self, len: usize) -> String {
        digest(&(&self.head, &self.messages[..len.min(self.messages.len())]))
    }

    /// Where `next` stops repeating `self`; `None` when it repeats.
    fn first_difference(&self, next: &Self) -> Option<Difference> {
        if self.head != next.head {
            return Some(Difference::Head);
        }
        (0..self.messages.len())
            .find(|&i| next.messages.get(i) != Some(&self.messages[i]))
            .map(Difference::Message)
    }
}

/// The first part of a request that changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Difference {
    /// The system blocks or the tools.
    Head,
    /// The message at this index.
    Message(usize),
}

fn digest(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    crate::sha256(&bytes)
}

impl Turn<'_> {
    /// Compares `request` with the session's previous request and keeps it
    /// as the new reference.
    pub(super) async fn check_prefix(&self, request: &Request) {
        let mut next = Prefix::of(request);
        if let Some(e) = &self.ephemeral
            && let Some(i) = e.index
            && let Some(slot) = next.messages.get_mut(i)
        {
            *slot = digest(&e.stored());
        }
        let previous = self.agent.swap_prefix(Some(next.clone()));
        let Some(previous) = previous else {
            return;
        };
        let Some(at) = previous.first_difference(&next) else {
            return;
        };
        let len = previous.messages.len();
        let event = CacheBreak {
            expected_prefix_hash: previous.hash(len),
            actual_prefix_hash: next.hash(len),
            first_diff_index: match at {
                Difference::Head => None,
                Difference::Message(i) => u32::try_from(i).ok(),
            },
        };
        if cfg!(debug_assertions) {
            tracing::error!(index = ?at, "cache.unexpected_break: an earlier part of the request changed");
        } else {
            tracing::warn!(index = ?at, "cache.unexpected_break: an earlier part of the request changed");
        }
        self.emit(Event::CacheUnexpectedBreak(event)).await;
    }
}

#[cfg(test)]
mod tests {
    use pmt_providers::request::{Message, Role};

    use super::*;

    fn request(texts: &[&str]) -> Request {
        Request {
            messages: texts
                .iter()
                .map(|t| Message::text(Role::User, *t))
                .collect(),
            ..Request::default()
        }
    }

    #[test]
    fn appending_keeps_the_prefix_and_an_edit_names_its_index() {
        let first = Prefix::of(&request(&["a", "b"]));
        assert_eq!(
            first.first_difference(&Prefix::of(&request(&["a", "b", "c"]))),
            None
        );
        assert_eq!(
            first.first_difference(&Prefix::of(&request(&["a", "B", "c"]))),
            Some(Difference::Message(1))
        );
        let mut marked = request(&["a", "b"]);
        marked.messages[1].cache_breakpoint = true;
        assert_eq!(
            first.first_difference(&Prefix::of(&marked)),
            None,
            "markers excluded"
        );
        let mut system = request(&["a", "b"]);
        system.system.push(pmt_providers::request::SystemBlock {
            text: "x".into(),
            cache_breakpoint: false,
        });
        assert_eq!(
            first.first_difference(&Prefix::of(&system)),
            Some(Difference::Head)
        );
    }
}
