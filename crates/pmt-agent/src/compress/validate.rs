//! Host validation of every compaction result (spec: context-compression,
//! Engine output is validated by the host).
//!
//! Whatever produced the compacted transcript (the built-in engine, a
//! plugin engine, a micro-fold or a proactive prune), it commits only when
//! it keeps strict role alternation where it wrote rows, puts every tool
//! result after its call, leaves no call without a result except on the
//! trailing in-flight assistant row, is smaller than the input, and keeps
//! the current user message intact. Rows the engine kept unchanged are not
//! judged for alternation: a stored transcript may already hold two user
//! rows in a row (an interrupted turn), which the request repair merges.

use std::collections::HashSet;

use pmt_store::{Message as Row, Role};

use super::assemble::{Assembled, Item};
use super::{is_real_user, rows_tokens};

/// Why a result was refused (the `compression.failed` message).
pub type Invalid = String;

/// Checks `output`, the compaction of its source rows.
///
/// # Errors
///
/// The first rule the output breaks.
pub fn validate(output: &Assembled<'_>) -> Result<(), Invalid> {
    alternation(output)?;
    calls_and_results(output)?;
    shrinks(output)?;
    keeps_current_user(output)
}

fn is_new(item: &Item) -> bool {
    matches!(item, Item::New(_))
}

/// No two adjacent user rows or assistant rows where the engine wrote one.
fn alternation(output: &Assembled<'_>) -> Result<(), Invalid> {
    for pair in output.items.windows(2) {
        let (a, b) = (output.role(&pair[0]), output.role(&pair[1]));
        let same = a == b && matches!(a, Role::User | Role::Assistant);
        if same && (is_new(&pair[0]) || is_new(&pair[1])) {
            return Err(format!("two adjacent {} rows", role_name(a)));
        }
    }
    Ok(())
}

const fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "system",
    }
}

/// Every result after its call; every call answered unless it is on the
/// trailing in-flight assistant row.
fn calls_and_results(output: &Assembled<'_>) -> Result<(), Invalid> {
    let last = output.items.len().saturating_sub(1);
    let mut called: HashSet<String> = HashSet::new();
    let mut unanswered: Vec<String> = Vec::new();
    for (i, item) in output.items.iter().enumerate() {
        match output.role(item) {
            Role::Tool => {
                let id = output.answers(item).unwrap_or_default();
                if !called.contains(&id) {
                    return Err(format!(
                        "the tool result for call '{id}' has no call before it"
                    ));
                }
                unanswered.retain(|open| *open != id);
            }
            Role::Assistant => {
                let ids = output.call_ids(item);
                called.extend(ids.iter().cloned());
                if i != last {
                    unanswered.extend(ids);
                }
            }
            Role::User | Role::System => {}
        }
    }
    match unanswered.first() {
        Some(id) => Err(format!("the tool call '{id}' has no result")),
        None => Ok(()),
    }
}

/// Fewer estimated tokens than the input.
fn shrinks(output: &Assembled<'_>) -> Result<(), Invalid> {
    let before = rows_tokens(output.source());
    let after: u64 = output.items.iter().map(|item| output.tokens(item)).sum();
    if after < before {
        Ok(())
    } else {
        Err(format!(
            "the result has {after} estimated tokens, not fewer than the input's {before}"
        ))
    }
}

/// The newest real user message is still there, word for word (a summary
/// merged in front of it leaves its text at the end).
fn keeps_current_user(output: &Assembled<'_>) -> Result<(), Invalid> {
    let rows: &[Row] = output.source();
    let Some(current) = rows.iter().rposition(is_real_user) else {
        return Ok(());
    };
    let text = rows[current].content.as_deref().unwrap_or_default();
    let kept = output.items.iter().any(|item| match item {
        Item::Kept(i) => *i == current,
        Item::New(row) => {
            row.role == Role::User && row.content.as_deref().is_some_and(|c| c.ends_with(text))
        }
    });
    if kept {
        Ok(())
    } else {
        Err("the current user message was changed or removed".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use pmt_core::id::MessageId;
    use pmt_store::NewMessage;

    use super::*;
    use crate::compress::assemble::assemble;
    use crate::compress::fixtures::{Build as _, row};
    use crate::compress::split::Split;

    fn long(role: Role, text: &str) -> Row {
        row(role, &format!("{text} {}", "x".repeat(400)))
    }

    fn new(role: Role, text: &str) -> Item {
        Item::New(Box::new(NewMessage::new(role, text)))
    }

    #[test]
    fn the_builtin_assembly_passes() {
        let rows = vec![
            long(Role::User, "old"),
            long(Role::Assistant, "old answer"),
            row(Role::User, "new"),
            row(Role::Assistant, "new answer"),
        ];
        let split = Split {
            head: 0..0,
            middle: 0..2,
            tail: 2..4,
        };
        assert_eq!(validate(&assemble(&rows, &split, "S")), Ok(()));
    }

    #[test]
    fn two_adjacent_assistant_rows_are_refused() {
        let rows = vec![
            long(Role::User, "old"),
            long(Role::Assistant, "old answer"),
            row(Role::User, "new"),
        ];
        let items = vec![
            new(Role::Assistant, "S"),
            new(Role::Assistant, "S2"),
            Item::Kept(2),
        ];
        let out = Assembled::from_items(&rows, items, MessageId::new());
        assert_eq!(
            validate(&out),
            Err("two adjacent assistant rows".to_owned())
        );
    }

    #[test]
    fn a_result_before_its_call_and_an_unanswered_call_are_refused() {
        let rows = vec![
            long(Role::User, "go"),
            long(Role::Assistant, "").with_call("c1"),
            long(Role::Tool, "out").answering("c1"),
            row(Role::Assistant, "done"),
            row(Role::User, "next"),
        ];
        let swapped = vec![Item::Kept(0), Item::Kept(2), Item::Kept(1), Item::Kept(4)];
        let out = Assembled::from_items(&rows, swapped, MessageId::new());
        assert!(
            validate(&out)
                .unwrap_err()
                .contains("has no call before it")
        );
        let dropped = vec![Item::Kept(1), new(Role::User, "next")];
        let out = Assembled::from_items(&rows, dropped, MessageId::new());
        assert!(validate(&out).unwrap_err().contains("has no result"));
    }

    #[test]
    fn a_result_that_grows_or_loses_the_current_message_is_refused() {
        let rows = vec![
            row(Role::User, "a"),
            row(Role::Assistant, "b"),
            row(Role::User, "c"),
        ];
        let grown = vec![new(Role::Assistant, &"S".repeat(100)), Item::Kept(2)];
        let out = Assembled::from_items(&rows, grown, MessageId::new());
        assert!(validate(&out).unwrap_err().contains("not fewer than"));

        let rows = vec![
            long(Role::User, "a"),
            long(Role::Assistant, "b"),
            row(Role::User, "c"),
        ];
        let lost = vec![new(Role::User, "S")];
        let out = Assembled::from_items(&rows, lost, MessageId::new());
        assert_eq!(
            validate(&out),
            Err("the current user message was changed or removed".to_owned())
        );
    }
}
