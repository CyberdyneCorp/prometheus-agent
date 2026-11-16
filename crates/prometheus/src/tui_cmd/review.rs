//! Inline diff review in the TUI (spec: tui, Inline diff review for file
//! edits): an approval that carries a file edit shows it as a numbered
//! diff and is answered `y` (accept all), `n` (reject), the hunk numbers
//! to accept (`hunks 1,3`, `1 3`) or `e` (edit then accept: the proposed
//! content goes to the composer and `Enter` sends the edited version).
//! The answer is `approval.respond`'s `{decision, accepted_hunks?,
//! replacement?}`.

use pmt_i18n::t;
use pmt_tui::diff::DiffHunk;
use serde_json::{Value, json};

use super::screen::Tui;

/// Whether an `approval.request`'s params carry an edit to review.
pub(super) fn has_diff(params: &Value) -> bool {
    params["diff"]["hunks"]
        .as_array()
        .is_some_and(|hunks| !hunks.is_empty())
}

/// The numbered hunks of an `approval.request`'s edit.
pub(super) fn hunks(params: &Value) -> Vec<DiffHunk> {
    params["diff"]["hunks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hunk| DiffHunk {
            header: hunk["header"].as_str().unwrap_or_default().to_owned(),
            lines: hunk["lines"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| l.as_str().map(str::to_owned))
                .collect(),
        })
        .collect()
}

/// The hunk numbers in `hunks 1,3`, `hunk 2` or `1 3`.
fn hunk_numbers(line: &str) -> Option<Vec<u32>> {
    let rest = line
        .strip_prefix("hunks")
        .or_else(|| line.strip_prefix("hunk"))
        .unwrap_or(line);
    let numbers: Option<Vec<u32>> = rest
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .map(|w| w.parse::<u32>().ok().filter(|n| *n > 0))
        .collect();
    numbers.filter(|n| !n.is_empty())
}

impl Tui<'_> {
    /// Answers the diff card on screen with `line`; `false` when it is no
    /// answer to it.
    pub(super) fn answer_diff(&mut self, line: &str) -> bool {
        let line = line.trim().to_lowercase();
        let answer = match line.as_str() {
            "y" | "yes" | "accept" | "o" | "once" => json!({ "decision": "once" }),
            "n" | "no" | "reject" | "d" | "deny" => json!({ "decision": "deny" }),
            "e" | "edit" => {
                self.start_edit();
                return true;
            }
            other => match hunk_numbers(other) {
                Some(accepted) => json!({ "decision": "once", "accepted_hunks": accepted }),
                None => return false,
            },
        };
        self.respond_front(answer);
        true
    }

    /// `edit then accept`: the proposed content goes to the composer.
    fn start_edit(&mut self) {
        let Some(asked) = self.asking.front() else {
            return;
        };
        let proposed = asked.call.params["diff"]["new_text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.editing = Some(asked.call.id.clone());
        self.fill = Some(proposed);
        self.show_question();
    }

    /// The edited content was sent: it is the replacement the tool writes.
    /// `false` when no edit is under way.
    pub(super) fn submit_edit(&mut self) -> bool {
        let Some(id) = self.editing.take() else {
            return false;
        };
        let text = self.app.composer.take();
        if self.asking.front().is_some_and(|a| a.call.id == id) {
            self.respond_front(json!({ "decision": "once", "replacement": text }));
        }
        true
    }

    /// `Esc` while editing: back to the card, the edit dropped.
    pub(super) fn cancel_edit(&mut self) -> bool {
        if self.editing.take().is_none() {
            return false;
        }
        self.app.composer.take();
        self.show_question();
        true
    }

    /// The card's hint while its content is being edited.
    pub(super) fn editing_hint(&self) -> Option<String> {
        let id = self.editing.as_ref()?;
        let asked = self.asking.front().filter(|a| a.call.id == *id)?;
        let path = asked.call.params["diff"]["path"]
            .as_str()
            .unwrap_or_default();
        Some(t!(self.strings(), "tui.diff_editing", 0 = path))
    }

    /// Puts what an answer left for the composer there.
    pub(super) fn apply_fill(&mut self) {
        if let Some(text) = self.fill.take() {
            self.app.composer.set(&text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::hunk_numbers;

    #[test]
    fn hunk_selections_read_in_every_form_and_junk_is_no_answer() {
        assert_eq!(hunk_numbers("hunks 1,3"), Some(vec![1, 3]));
        assert_eq!(hunk_numbers("1 3"), Some(vec![1, 3]));
        assert_eq!(hunk_numbers("hunk 2"), Some(vec![2]));
        assert_eq!(hunk_numbers("hunks"), None);
        assert_eq!(hunk_numbers("0"), None);
        assert_eq!(hunk_numbers("please"), None);
    }
}
