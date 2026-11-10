//! Opt-in micro-compaction (spec: context-compression, Opt-in
//! micro-compaction).
//!
//! With `compression.micro_compact`, every `micro_compact_every_n_turns`
//! completed turns one assistant exchange (the assistant and tool rows
//! answering one user message) of the compressible middle is folded into a
//! single rolling summary row, written by the summarizer as an update of
//! the previous rolling summary. The newest rolling row takes the folded
//! exchange's place; the previous one becomes a one-line stub, so roles
//! keep alternating and only the newest rolling summary is in the context.
//! User messages, the protected head and the protected tail are never
//! folded. The caller skips an exchange after [`MAX_FAILURES`] failures on
//! it in a row.

use std::collections::HashSet;
use std::hash::BuildHasher;
use std::ops::Range;

use pmt_core::error::Error;
use pmt_core::id::MessageId;
use pmt_providers::request::{Message, Request, Role as WireRole, SystemBlock};
use pmt_store::{Message as Row, NewMessage, Role};
use serde_json::json;

use super::assemble::{Assembled, Item};
use super::compactor::{Compactor, Run};
use super::summary;
use super::validate::validate;
use super::{flag, is_summary, tokens};

/// Metadata flag of the rolling summary row.
pub const ROLLING_FLAG: &str = "micro_summary";
/// Metadata flag of a former rolling row.
pub const FOLDED_FLAG: &str = "micro_folded";
/// The first line of the rolling summary row.
pub const ROLLING_HEADER: &str = "[ROLLING SUMMARY OF EARLIER EXCHANGES - REFERENCE ONLY]";
/// What a former rolling row says.
pub const STUB: &str = "[earlier exchange folded into the rolling summary]";
/// Failures in a row after which an exchange is skipped.
pub const MAX_FAILURES: u32 = 3;

const SYSTEM: &str = "You keep a rolling summary of an AI agent's earlier work so older \
exchanges can be removed from its context. Update the summary with the exchange below: keep \
file paths, commands, identifiers, numbers, errors and decisions; drop what is obsolete; stay \
short. Write only the updated summary, without a heading. Do not call tools.";

/// How one micro-compaction pass ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Micro {
    /// The exchange starting at this row was folded.
    Folded(MessageId),
    /// No exchange can be folded.
    Nothing,
    /// Folding the exchange starting at this row failed.
    Failed(MessageId, String),
    /// Another compaction holds the session's lock.
    Busy,
}

fn is_rolling(row: &Row) -> bool {
    flag(row, ROLLING_FLAG) || flag(row, FOLDED_FLAG)
}

/// The oldest exchange inside `middle` not in `skip`: a run of assistant
/// and tool rows that starts with an assistant reply and is followed by a
/// user message.
#[must_use]
pub fn next_exchange<S: BuildHasher>(
    rows: &[Row],
    middle: Range<usize>,
    skip: &HashSet<MessageId, S>,
) -> Option<Range<usize>> {
    let mut i = middle.start;
    while i < middle.end {
        let starts =
            rows[i].role == Role::Assistant && !is_rolling(&rows[i]) && !is_summary(&rows[i]);
        let mut end = i;
        while end < middle.end && matches!(rows[end].role, Role::Assistant | Role::Tool) {
            end += 1;
        }
        let followed = rows.get(end).is_some_and(|r| r.role == Role::User);
        let whole = (i..end).all(|k| !is_rolling(&rows[k]) && !is_summary(&rows[k]));
        if starts && end > i && followed && whole && !skip.contains(&rows[i].id) {
            return Some(i..end);
        }
        i = end.max(i + 1);
    }
    None
}

impl Compactor<'_> {
    /// One micro-compaction pass (`run` sizes the protected tail), skipping
    /// the exchanges that start at a row of `skip`.
    ///
    /// # Errors
    ///
    /// Store failures; a failed fold is [`Micro::Failed`].
    pub async fn micro_compact<S: BuildHasher + Sync>(
        &self,
        run: &Run,
        skip: &HashSet<MessageId, S>,
    ) -> Result<Micro, Error> {
        let Some(lock) = self.lock()? else {
            return Ok(Micro::Busy);
        };
        let result = self.fold(run, &lock, skip).await;
        self.unlock(&lock);
        result
    }

    async fn fold<S: BuildHasher + Sync>(
        &self,
        run: &Run,
        lock: &pmt_store::Lease,
        skip: &HashSet<MessageId, S>,
    ) -> Result<Micro, Error> {
        let plan = self.plan(run)?;
        let rows = &plan.rows;
        let Some(exchange) = next_exchange(rows, plan.split.middle.clone(), skip) else {
            return Ok(Micro::Nothing);
        };
        let first = rows[exchange.start].id;
        let previous = rows.iter().position(|r| flag(r, ROLLING_FLAG));
        let previous_text = previous
            .and_then(|i| rows[i].content.as_deref())
            .map(|t| t.trim_start_matches(ROLLING_HEADER).trim().to_owned());
        let request = request(
            previous_text.as_deref(),
            &summary::render(&rows[exchange.clone()]),
        );
        let text = match self.summarize_request(&request).await {
            Ok(text) => text,
            Err(failure) => return Ok(Micro::Failed(first, failure.message)),
        };
        let assembled = folded(rows, exchange, previous, text.trim());
        if let Err(why) = validate(&assembled) {
            return Ok(Micro::Failed(first, why));
        }
        Ok(match self.apply(lock, &assembled, &[])? {
            Ok(_) => Micro::Folded(first),
            Err(conflict) => Micro::Failed(first, conflict),
        })
    }
}

/// `rows` with `exchange` replaced by the new rolling row and the
/// `previous` rolling row by a stub.
fn folded<'a>(
    rows: &'a [Row],
    exchange: Range<usize>,
    previous: Option<usize>,
    text: &str,
) -> Assembled<'a> {
    let mut rolling = NewMessage::new(Role::Assistant, format!("{ROLLING_HEADER}\n{text}"));
    rolling.metadata = Some(json!({ ROLLING_FLAG: true }));
    let rolling_id = rolling.id;
    let mut rolling = Some(rolling);
    let mut items = Vec::with_capacity(rows.len());
    for i in 0..rows.len() {
        if Some(i) == previous {
            let mut stub = NewMessage::new(Role::Assistant, STUB);
            stub.metadata = Some(json!({ FOLDED_FLAG: true }));
            items.push(Item::New(Box::new(stub)));
        } else if exchange.contains(&i) {
            if let Some(row) = rolling.take() {
                items.push(Item::New(Box::new(row)));
            }
        } else {
            items.push(Item::Kept(i));
        }
    }
    Assembled::from_items(rows, items, rolling_id)
}

/// The summarizer request folding `exchange` into `previous`.
fn request(previous: Option<&str>, exchange: &str) -> Request {
    let mut text = String::new();
    if let Some(previous) = previous {
        text.push_str("ROLLING SUMMARY SO FAR:\n\n");
        text.push_str(previous);
        text.push_str("\n\n");
    }
    text.push_str("EXCHANGE TO FOLD IN:\n\n");
    text.push_str(exchange);
    let budget = (tokens(exchange) + previous.map_or(0, tokens)).clamp(256, 2_000);
    Request {
        system: vec![SystemBlock {
            text: SYSTEM.to_owned(),
            cache_breakpoint: false,
        }],
        messages: vec![Message::text(WireRole::User, text)],
        max_output_tokens: Some(budget),
        ..Request::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::fixtures::{Build as _, row};

    fn transcript() -> Vec<Row> {
        vec![
            row(Role::User, "first"),
            row(Role::Assistant, "").with_call("c1"),
            row(Role::Tool, "out").answering("c1"),
            row(Role::Assistant, "first answer"),
            row(Role::User, "second"),
            row(Role::Assistant, "second answer"),
            row(Role::User, "third"),
        ]
    }

    #[test]
    fn the_oldest_whole_exchange_is_next_and_a_skipped_one_is_passed_over() {
        let rows = transcript();
        assert_eq!(next_exchange(&rows, 0..6, &HashSet::new()), Some(1..4));
        let skip = HashSet::from([rows[1].id]);
        assert_eq!(next_exchange(&rows, 0..6, &skip), Some(5..6));
        // An exchange running into the protected tail is not whole.
        assert_eq!(next_exchange(&rows, 4..5, &HashSet::new()), None);
    }

    #[test]
    fn the_new_rolling_row_replaces_the_exchange_and_the_old_one_becomes_a_stub() {
        let mut rows = transcript();
        rows[3] = row(
            Role::Assistant,
            "[ROLLING SUMMARY OF EARLIER EXCHANGES - REFERENCE ONLY]\nold",
        )
        .meta(ROLLING_FLAG, json!(true));
        rows.remove(2);
        rows.remove(1);
        // user, rolling, user "second", assistant, user "third"
        let out = folded(&rows, 3..4, Some(1), "S2");
        let view = out.view();
        assert_eq!(view[1], (Role::Assistant, STUB.to_owned()));
        assert_eq!(view[3].1, format!("{ROLLING_HEADER}\nS2"));
        let users: Vec<&str> = view
            .iter()
            .filter(|(r, _)| *r == Role::User)
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(users, ["first", "second", "third"]);
    }
}
