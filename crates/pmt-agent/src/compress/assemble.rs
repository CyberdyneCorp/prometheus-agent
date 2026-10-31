//! The compacted transcript and the rows a commit changes (spec:
//! context-compression, Handoff summary shape; Orphan handling stubs
//! missing results).
//!
//! The summary goes between the head and the tail with a role that keeps
//! strict alternation (user and tool rows on one side, assistant rows on
//! the other); when neither role fits it is merged into the first tail
//! row. Then every tool result whose call is gone is dropped and every
//! call whose result is gone (except on the trailing in-flight assistant
//! row) gets the stub result [`FOLDED`], so the record of the action
//! survives. [`Assembled::commit`] turns the result into the store's
//! compaction: the rows between the unchanged prefix and suffix are
//! replaced by the assembled rows between them.

use std::collections::HashSet;

use pmt_core::id::MessageId;
use pmt_store::{Message as Row, NewMessage, Role};
use serde_json::{Value, json};

use super::SUMMARY_FLAG;
use super::split::Split;

/// The stub result of a call whose result was summarized.
pub const FOLDED: &str = "[result folded into the context summary]";

/// One row of the assembled transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// The stored active row at this index, unchanged.
    Kept(usize),
    /// A new row.
    New(Box<NewMessage>),
}

/// The compacted transcript over the stored rows it came from.
#[derive(Debug, Clone)]
pub struct Assembled<'a> {
    rows: &'a [Row],
    /// The rows in order.
    pub items: Vec<Item>,
    /// The id of the row carrying the summary.
    pub summary_id: MessageId,
}

/// Which side of the alternation a role is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    User,
    Assistant,
}

const fn side(role: Role) -> Option<Side> {
    match role {
        Role::User | Role::Tool => Some(Side::User),
        Role::Assistant => Some(Side::Assistant),
        Role::System => None,
    }
}

/// A stored row as a new one with a fresh id (the content stays).
#[must_use]
pub fn copy(row: &Row) -> NewMessage {
    NewMessage {
        id: MessageId::new(),
        role: row.role,
        content: row.content.clone(),
        tool_calls: row.tool_calls.clone(),
        reasoning: row.reasoning.clone(),
        provider_payload: row.provider_payload.clone(),
        tool_call_id: row.tool_call_id.clone(),
        tool_name: row.tool_name.clone(),
        metadata: row.metadata.clone(),
    }
}

fn mark_summary(row: &mut NewMessage) {
    let mut map = row
        .metadata
        .take()
        .and_then(|m| m.as_object().cloned())
        .unwrap_or_default();
    map.insert(SUMMARY_FLAG.to_owned(), Value::Bool(true));
    row.metadata = Some(Value::Object(map));
}

/// Head, summary and tail of `rows` split by `split`, orphans fixed.
#[must_use]
pub fn assemble<'a>(rows: &'a [Row], split: &Split, summary: &str) -> Assembled<'a> {
    let mut assembled = Assembled {
        rows,
        items: split
            .head
            .clone()
            .chain(split.tail.clone())
            .map(Item::Kept)
            .collect(),
        summary_id: MessageId::new(),
    };
    assembled.fix_orphans();
    // The summary goes before the first tail row (after any stub that
    // answers a head call).
    let at = assembled
        .items
        .iter()
        .position(|item| matches!(item, Item::Kept(i) if *i >= split.tail.start))
        .unwrap_or(assembled.items.len());
    assembled.place(at, summary);
    assembled
}

impl Assembled<'_> {
    /// Inserts the summary at `at` with a role that keeps alternation, or
    /// merges it into the row at `at` when neither role fits.
    fn place(&mut self, at: usize, summary: &str) {
        let before = at
            .checked_sub(1)
            .and_then(|i| side(self.role(&self.items[i])));
        let after = self.items.get(at).and_then(|item| side(self.role(item)));
        let role = [Role::User, Role::Assistant]
            .into_iter()
            .find(|r| side(*r) != before && side(*r) != after);
        let mut row = match (role, &self.items.get(at)) {
            (Some(role), _) => NewMessage::new(role, summary),
            (None, Some(Item::Kept(i))) => {
                let first = &self.rows[*i];
                let mut row = copy(first);
                let original = first.content.as_deref().unwrap_or_default();
                row.content = Some(format!("{summary}\n\n{original}"));
                self.items.remove(at);
                row
            }
            // Both neighbours exist when no role fits, and the one after
            // is a stored tail row.
            (None, _) => NewMessage::new(Role::User, summary),
        };
        mark_summary(&mut row);
        self.summary_id = row.id;
        self.items.insert(at, Item::New(Box::new(row)));
    }

    fn role(&self, item: &Item) -> Role {
        match item {
            Item::Kept(i) => self.rows[*i].role,
            Item::New(row) => row.role,
        }
    }

    fn call_ids(&self, item: &Item) -> Vec<String> {
        let calls = match item {
            Item::Kept(i) => self.rows[*i].tool_calls.as_ref(),
            Item::New(row) => row.tool_calls.as_ref(),
        };
        calls
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    fn answers(&self, item: &Item) -> Option<String> {
        match item {
            Item::Kept(i) => self.rows[*i].tool_call_id.clone(),
            Item::New(row) => row.tool_call_id.clone(),
        }
    }

    /// Drops results without their call and stubs calls without their
    /// result (not on the trailing in-flight assistant row).
    fn fix_orphans(&mut self) {
        let items = std::mem::take(&mut self.items);
        let last = items.len().saturating_sub(1);
        let mut out = Vec::with_capacity(items.len());
        let mut open: Vec<String> = Vec::new();
        let mut known: HashSet<String> = HashSet::new();
        for (i, item) in items.into_iter().enumerate() {
            if self.role(&item) == Role::Tool {
                let id = self.answers(&item).unwrap_or_default();
                if known.contains(&id) {
                    open.retain(|o| *o != id);
                    out.push(item);
                }
                continue;
            }
            out.extend(open.drain(..).map(stub));
            if self.role(&item) == Role::Assistant {
                let ids = self.call_ids(&item);
                known.extend(ids.iter().cloned());
                if i != last {
                    open = ids;
                }
            }
            out.push(item);
        }
        out.extend(open.drain(..).map(stub));
        self.items = out;
    }

    /// The store compaction: the rows between the unchanged prefix and
    /// suffix are replaced by the assembled rows between them (unchanged
    /// rows there are copied with a fresh id).
    #[must_use]
    pub fn commit(&self) -> (Vec<MessageId>, Vec<NewMessage>) {
        let rows = self.rows.len();
        let prefix = self
            .items
            .iter()
            .enumerate()
            .take_while(|(k, item)| **item == Item::Kept(*k))
            .count();
        let suffix = self
            .items
            .iter()
            .rev()
            .enumerate()
            .take_while(|(j, item)| rows >= j + 1 + prefix && **item == Item::Kept(rows - 1 - j))
            .count();
        let replaced = self.rows[prefix..rows - suffix]
            .iter()
            .map(|r| r.id)
            .collect();
        let replacements = self.items[prefix..self.items.len() - suffix]
            .iter()
            .map(|item| match item {
                Item::Kept(i) => copy(&self.rows[*i]),
                Item::New(row) => (**row).clone(),
            })
            .collect();
        (replaced, replacements)
    }

    /// The assembled rows' roles and texts (previews and tests).
    #[must_use]
    pub fn view(&self) -> Vec<(Role, String)> {
        self.items
            .iter()
            .map(|item| match item {
                Item::Kept(i) => (
                    self.rows[*i].role,
                    self.rows[*i].content.clone().unwrap_or_default(),
                ),
                Item::New(row) => (row.role, row.content.clone().unwrap_or_default()),
            })
            .collect()
    }
}

fn stub(call_id: String) -> Item {
    let mut row = NewMessage::new(Role::Tool, FOLDED);
    row.tool_call_id = Some(call_id);
    row.metadata = Some(json!({ "status": "ok", "is_error": false, "folded": true }));
    Item::New(Box::new(row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::fixtures::{Build as _, row};

    fn split(head: usize, tail: usize, len: usize) -> Split {
        Split {
            head: 0..head,
            middle: head..tail,
            tail: tail..len,
        }
    }

    fn roles(a: &Assembled<'_>) -> Vec<Role> {
        a.view().into_iter().map(|(r, _)| r).collect()
    }

    fn alternates(a: &Assembled<'_>) -> bool {
        a.view()
            .windows(2)
            .all(|w| side(w[0].0) != side(w[1].0) || w[1].0 == Role::Tool)
    }

    #[test]
    fn a_tail_starting_with_a_user_row_gets_an_assistant_summary() {
        let rows = vec![
            row(Role::User, "old"),
            row(Role::Assistant, "old answer"),
            row(Role::User, "new"),
            row(Role::Assistant, "new answer"),
        ];
        let a = assemble(&rows, &split(0, 2, 4), "S");
        assert_eq!(roles(&a), [Role::Assistant, Role::User, Role::Assistant]);
        assert!(alternates(&a));
    }

    #[test]
    fn with_no_fitting_role_the_summary_merges_into_the_first_tail_row() {
        // Head ends with the assistant, tail starts with the user.
        let rows = vec![
            row(Role::User, "first"),
            row(Role::Assistant, "first answer"),
            row(Role::User, "middle"),
            row(Role::Assistant, "middle answer"),
            row(Role::User, "current"),
        ];
        let a = assemble(&rows, &split(2, 4, 5), "S");
        let view = a.view();
        assert_eq!(roles(&a), [Role::User, Role::Assistant, Role::User]);
        assert_eq!(view[2].1, "S\n\ncurrent");
        let (replaced, replacements) = a.commit();
        assert_eq!(replaced, [rows[2].id, rows[3].id, rows[4].id]);
        assert_eq!(replacements.len(), 1);
        assert_eq!(replacements[0].id, a.summary_id);
    }

    #[test]
    fn a_call_whose_result_was_summarized_gets_the_stub() {
        let rows = vec![
            row(Role::User, "go"),
            row(Role::Assistant, "").with_call("c1"),
            row(Role::Tool, "result").answering("c1"),
            row(Role::Assistant, "done"),
            row(Role::User, "next"),
        ];
        // The head keeps the call; its result is summarized.
        let a = assemble(&rows, &split(2, 4, 5), "S");
        let view = a.view();
        assert_eq!(view[1].0, Role::Assistant);
        assert_eq!(view[2], (Role::Tool, FOLDED.to_owned()));
        assert_eq!(view[3], (Role::Assistant, "S".to_owned()));
        assert_eq!(view[4], (Role::User, "next".to_owned()));
        assert!(alternates(&a));
    }

    #[test]
    fn a_result_whose_call_was_summarized_is_dropped() {
        let rows = vec![
            row(Role::User, "go"),
            row(Role::Assistant, "").with_call("c1"),
            row(Role::Tool, "result").answering("c1"),
            row(Role::Assistant, "done"),
        ];
        let a = assemble(&rows, &split(0, 2, 4), "S");
        assert_eq!(roles(&a), [Role::User, Role::Assistant]);
        let (replaced, _) = a.commit();
        assert_eq!(replaced, [rows[0].id, rows[1].id, rows[2].id]);
    }

    #[test]
    fn the_trailing_in_flight_call_is_not_stubbed() {
        let rows = vec![
            row(Role::User, "old"),
            row(Role::Assistant, "old answer"),
            row(Role::User, "go"),
            row(Role::Assistant, "").with_call("c9"),
        ];
        let a = assemble(&rows, &split(0, 2, 4), "S");
        assert_eq!(roles(&a), [Role::Assistant, Role::User, Role::Assistant]);
    }

    #[test]
    fn the_commit_replaces_only_the_middle() {
        let rows: Vec<Row> = (0..6)
            .map(|i| {
                let role = if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                };
                row(role, &format!("m{i}"))
            })
            .collect();
        let a = assemble(&rows, &split(1, 4, 6), "S");
        let (replaced, replacements) = a.commit();
        assert_eq!(replaced, [rows[1].id, rows[2].id, rows[3].id]);
        assert_eq!(replacements.len(), 1);
        assert_eq!(replacements[0].role, Role::Assistant);
        assert_eq!(
            replacements[0].metadata.as_ref().unwrap()[SUMMARY_FLAG],
            Value::Bool(true)
        );
    }
}
