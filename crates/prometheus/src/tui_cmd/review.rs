//! Inline diff review in the TUI (spec: tui, Inline diff review for file
//! edits): an approval that carries a file edit shows it as a numbered
//! diff and is answered `y` (accept all), `n` (reject), the hunk numbers
//! to accept (`hunks 1,3`, `1 3`) or `e` (edit then accept: the proposed
//! content goes to the composer and `Enter` sends the edited version).
//! The answer is `approval.respond`'s `{decision, accepted_hunks?,
//! replacement?}`.

use std::ops::Range;
use std::path::Path;

use pmt_i18n::t;
use pmt_tools::files::syntax::{self, Class, LineClasses};
use pmt_tui::diff::{DiffHunk, Syntax};
use serde_json::{Value, json};

use super::screen::Tui;

/// Whether an `approval.request`'s params carry an edit to review.
pub(super) fn has_diff(params: &Value) -> bool {
    params["diff"]["hunks"]
        .as_array()
        .is_some_and(|hunks| !hunks.is_empty())
}

/// The numbered hunks of an `approval.request`'s edit, their code
/// highlighted by the edited file's language.
pub(super) fn hunks(params: &Value) -> Vec<DiffHunk> {
    let diff = &params["diff"];
    let path = Path::new(diff["path"].as_str().unwrap_or_default());
    let old = syntax::classes(path, diff["old_text"].as_str().unwrap_or_default());
    let new = syntax::classes(path, diff["new_text"].as_str().unwrap_or_default());
    diff["hunks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hunk| {
            let header = hunk["header"].as_str().unwrap_or_default().to_owned();
            let lines: Vec<String> = hunk["lines"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| l.as_str().map(str::to_owned))
                .collect();
            let syntax = hunk_syntax(&header, &lines, (&old, &new));
            DiffHunk {
                header,
                lines,
                syntax,
            }
        })
        .collect()
}

/// The syntax runs of a hunk's lines: a removed line's from the old
/// text, the others' from the new one, found by the line numbers the
/// header starts at (`@@ -<old>,<n> +<new>,<n> @@`).
fn hunk_syntax(
    header: &str,
    lines: &[String],
    (old, new): (&[LineClasses], &[LineClasses]),
) -> Vec<Vec<(Range<usize>, Syntax)>> {
    let Some((mut at_old, mut at_new)) = starts(header) else {
        return Vec::new();
    };
    let runs_of = |classes: &[LineClasses], at: &mut usize| {
        let runs = classes.get(*at).map_or_else(Vec::new, |runs| {
            runs.iter().map(|(r, c)| (r.clone(), shown(*c))).collect()
        });
        *at += 1;
        runs
    };
    lines
        .iter()
        .map(|line| match line.chars().next() {
            Some('-') => runs_of(old, &mut at_old),
            Some('+') => runs_of(new, &mut at_new),
            _ => {
                at_old += 1;
                runs_of(new, &mut at_new)
            }
        })
        .collect()
}

/// The 0-based first old and new lines of a hunk header.
fn starts(header: &str) -> Option<(usize, usize)> {
    let mut words = header.split_whitespace().skip(1);
    let mut start = |sign: char| -> Option<usize> {
        let word = words.next()?.strip_prefix(sign)?;
        let line: usize = word.split(',').next()?.parse().ok()?;
        // A hunk of no lines (`-0,0`, a new file) starts before line 1.
        Some(line.saturating_sub(1))
    };
    Some((start('-')?, start('+')?))
}

const fn shown(class: Class) -> Syntax {
    match class {
        Class::Keyword => Syntax::Keyword,
        Class::String => Syntax::String,
        Class::Comment => Syntax::Comment,
        Class::Constant => Syntax::Constant,
        Class::Type => Syntax::Type,
    }
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
    use pmt_tui::diff::Syntax;
    use serde_json::json;

    use super::{hunk_numbers, hunks};

    #[test]
    fn removed_lines_take_the_old_files_syntax_and_others_the_new_ones() {
        let params = json!({ "diff": {
            "path": "/w/lib.rs",
            "old_text": "// a\nfn f() {}\nlet x = 1;\n",
            "new_text": "// a\nfn f() {}\nlet x = \"s\";\n",
            "hunks": [{
                "header": "@@ -2,2 +2,2 @@",
                "lines": [" fn f() {}", "-let x = 1;", "+let x = \"s\";"],
            }],
        }});
        let hunk = &hunks(&params)[0];
        assert_eq!(hunk.syntax[0], [(0..2, Syntax::Keyword)]);
        assert_eq!(
            hunk.syntax[1],
            [(0..3, Syntax::Keyword), (8..9, Syntax::Constant)]
        );
        assert_eq!(
            hunk.syntax[2],
            [(0..3, Syntax::Keyword), (8..11, Syntax::String)]
        );
    }

    #[test]
    fn a_file_in_an_unknown_language_shows_its_diff_uncoloured() {
        let params = json!({ "diff": {
            "path": "/w/notes.txt",
            "old_text": "a\n",
            "new_text": "b\n",
            "hunks": [{ "header": "@@ -1,1 +1,1 @@", "lines": ["-a", "+b"] }],
        }});
        assert!(hunks(&params)[0].syntax.iter().all(Vec::is_empty));
    }

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
