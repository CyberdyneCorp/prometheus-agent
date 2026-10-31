//! Where the summarized middle starts and ends (spec: context-compression,
//! Protected head; Token-budgeted tail; Manual compaction command).
//!
//! The system prompt is never a stored row, so it is always protected and
//! never counted. On a session's first compaction `protect_first_n`
//! non-system rows after it stay verbatim; once a summary exists only the
//! system message is head-protected. The tail is chosen walking backward
//! under a token budget, extended to hold at least `min_tail_user_messages`
//! real user messages (or exactly the last N user exchanges for
//! `/compress here N`, which protects no head). Neither boundary ever separates an assistant tool
//! call from its results: the head grows over trailing results, the tail
//! start moves back to the assistant row.

use std::ops::Range;

use pmt_store::{Message as Row, Role};

use super::{is_real_user, is_summary, row_tokens};

/// How to split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// The session has no compaction summary yet.
    pub first_compaction: bool,
    /// `compression.protect_first_n`.
    pub protect_first_n: u32,
    /// The tail's token budget.
    pub tail_budget: u64,
    /// `compression.min_tail_user_messages`.
    pub min_tail_users: u32,
    /// `/compress here N`: keep exactly the last N user exchanges.
    pub keep_exchanges: Option<u32>,
}

/// The three regions of the active rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    /// Rows kept verbatim at the start.
    pub head: Range<usize>,
    /// Rows summarized (the previous summary among them).
    pub middle: Range<usize>,
    /// Rows kept verbatim at the end.
    pub tail: Range<usize>,
}

impl Split {
    /// The previous summary row inside the middle, if any.
    #[must_use]
    pub fn previous_summary(&self, rows: &[Row]) -> Option<usize> {
        self.middle.clone().rev().find(|&i| is_summary(&rows[i]))
    }

    /// Whether the middle holds anything besides a previous summary.
    #[must_use]
    pub fn has_work(&self, rows: &[Row]) -> bool {
        self.middle.clone().any(|i| !is_summary(&rows[i]))
    }
}

/// Splits `rows` (the active transcript, in order).
#[must_use]
pub fn split(rows: &[Row], options: &Options) -> Split {
    let head = head_end(rows, options);
    let wanted = match options.keep_exchanges {
        Some(n) => nth_last_user(rows, n).unwrap_or(0),
        None => budgeted_start(rows, options),
    };
    let tail = align_back(rows, wanted).max(head);
    Split {
        head: 0..head,
        middle: head..tail,
        tail: tail..rows.len(),
    }
}

/// The end of the protected head.
fn head_end(rows: &[Row], options: &Options) -> usize {
    let mut end = 0;
    while end < rows.len() && rows[end].role == Role::System {
        end += 1;
    }
    // `/compress here N` summarizes everything before the kept exchanges.
    if !options.first_compaction || options.keep_exchanges.is_some() {
        return end;
    }
    let mut counted = 0;
    while end < rows.len() && counted < options.protect_first_n {
        if rows[end].role != Role::System {
            counted += 1;
        }
        end += 1;
    }
    // The results of a protected call stay with it.
    while end < rows.len() && rows[end].role == Role::Tool {
        end += 1;
    }
    end
}

/// The index of the `n`-th real user row from the end.
fn nth_last_user(rows: &[Row], n: u32) -> Option<usize> {
    let n = usize::try_from(n).ok()?.max(1);
    rows.iter()
        .enumerate()
        .rev()
        .filter(|(_, row)| is_real_user(row))
        .nth(n - 1)
        .map(|(i, _)| i)
}

/// The tail start under the token budget, moved back to hold the minimum
/// of real user messages.
fn budgeted_start(rows: &[Row], options: &Options) -> usize {
    let mut start = rows.len();
    let mut used = 0;
    while start > 0 {
        let cost = row_tokens(&rows[start - 1]);
        if used + cost > options.tail_budget {
            break;
        }
        used += cost;
        start -= 1;
    }
    if options.min_tail_users == 0 {
        return start;
    }
    nth_last_user(rows, options.min_tail_users).map_or(0, |user| start.min(user))
}

/// Moves a tail start that would orphan tool results back to their
/// assistant row.
fn align_back(rows: &[Row], mut start: usize) -> usize {
    while start > 0 && start < rows.len() && rows[start].role == Role::Tool {
        start -= 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use pmt_store::Role;

    use super::*;
    use crate::compress::fixtures::{Build as _, row};

    fn options(tail_budget: u64) -> Options {
        Options {
            first_compaction: true,
            protect_first_n: 3,
            tail_budget,
            min_tail_users: 1,
            keep_exchanges: None,
        }
    }

    /// user, assistant (call), tool, assistant, ... repeated `n` times.
    fn exchanges(n: usize) -> Vec<Row> {
        let mut rows = Vec::new();
        for i in 0..n {
            rows.push(row(Role::User, &format!("question {i}")));
            rows.push(row(Role::Assistant, &format!("call {i}")).with_call(&format!("c{i}")));
            rows.push(row(Role::Tool, &"x".repeat(400)).answering(&format!("c{i}")));
            rows.push(row(Role::Assistant, &format!("answer {i}")));
        }
        rows
    }

    #[test]
    fn the_head_protects_n_rows_only_on_the_first_compaction() {
        let rows = exchanges(5);
        let first = split(&rows, &options(0));
        // user, assistant(call), tool -> 3 rows, already a whole group.
        assert_eq!(first.head, 0..3);
        let later = split(
            &rows,
            &Options {
                first_compaction: false,
                ..options(0)
            },
        );
        assert_eq!(later.head, 0..0);
    }

    #[test]
    fn a_head_ending_on_a_call_keeps_its_results() {
        let rows = exchanges(5);
        let two = Options {
            protect_first_n: 2,
            ..options(0)
        };
        assert_eq!(split(&rows, &two).head, 0..3);
    }

    #[test]
    fn the_tail_never_splits_a_call_from_its_results() {
        let rows = exchanges(5);
        // Budget for the last answer and the tool row before it only.
        let budget = row_tokens(&rows[19]) + row_tokens(&rows[18]);
        let s = split(
            &rows,
            &Options {
                min_tail_users: 0,
                ..options(budget)
            },
        );
        assert_eq!(rows[s.tail.start].role, Role::Assistant);
        assert_eq!(s.tail, 17..20, "moved back before the assistant call");
    }

    #[test]
    fn the_tail_holds_the_minimum_of_user_messages_past_the_budget() {
        let rows = exchanges(5);
        let s = split(&rows, &options(1));
        assert_eq!(s.tail, 16..20);
        assert!(is_real_user(&rows[s.tail.start]));
        assert_eq!(s.middle, 3..16);
    }

    #[test]
    fn here_keeps_exactly_the_last_n_exchanges() {
        let rows = exchanges(30);
        let s = split(
            &rows,
            &Options {
                keep_exchanges: Some(2),
                ..options(1_000_000)
            },
        );
        assert_eq!(s.tail, 112..120);
        assert_eq!(rows[s.tail.start].content.as_deref(), Some("question 28"));
    }
}
