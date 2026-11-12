//! The live session switcher (spec: tui, Live session switcher): an
//! overlay listing the sessions this TUI is attached to, the shown one
//! marked and running ones flagged, and a last `+new` row. On `+new` the
//! person types the new session's first prompt (with `Tab` cycling the
//! model for that session only) and `Enter` starts it.

use ratatui::text::{Line, Span};

use crate::theme::Theme;
use crate::wrap::truncate;

/// One live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// `ses_<ULID>`.
    pub id: String,
    /// Its title, when it has one.
    pub title: Option<String>,
    /// A turn runs in it.
    pub busy: bool,
    /// The screen shows it.
    pub shown: bool,
}

/// The first prompt of a session started from `+new`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    /// What is typed.
    pub prompt: String,
    /// The model chosen with `Tab` (the profile's default when `None`).
    pub model: Option<String>,
}

/// The open switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switcher {
    /// The live sessions.
    pub rows: Vec<Row>,
    /// The highlighted row; `rows.len()` is `+new`.
    pub selected: usize,
    /// The `+new` prompt being typed.
    pub draft: Option<Draft>,
}

impl Switcher {
    /// The switcher over `rows`, the shown session highlighted.
    #[must_use]
    pub fn new(rows: Vec<Row>) -> Self {
        let selected = rows.iter().position(|r| r.shown).unwrap_or(0);
        Self {
            rows,
            selected,
            draft: None,
        }
    }

    /// Moves the highlight `by` rows (`+new` included), leaving the draft.
    pub fn move_selection(&mut self, by: isize) {
        self.selected = self.selected.saturating_add_signed(by).min(self.rows.len());
        if !self.on_new() {
            self.draft = None;
        }
    }

    /// `+new` is highlighted.
    #[must_use]
    pub fn on_new(&self) -> bool {
        self.selected == self.rows.len()
    }

    /// The highlighted live session.
    #[must_use]
    pub fn current(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// Types `c` into the `+new` prompt (starting it).
    pub fn type_char(&mut self, c: char) {
        if self.on_new() {
            self.draft.get_or_insert_default().prompt.push(c);
        }
    }

    /// Picks the model after the draft's in `models` (wrapping; the first
    /// when none was picked).
    pub fn next_model(&mut self, models: &[String]) {
        if !self.on_new() || models.is_empty() {
            return;
        }
        let draft = self.draft.get_or_insert_default();
        let next = draft
            .model
            .as_ref()
            .and_then(|m| models.iter().position(|x| x == m))
            .map_or(0, |at| (at + 1) % models.len());
        draft.model = Some(models[next].clone());
    }

    /// The overlay's lines for `width` columns and at most `rows` lines.
    #[must_use]
    pub fn lines(
        &self,
        width: u16,
        rows: usize,
        theme: &Theme,
        strings: &pmt_i18n::Strings,
    ) -> Vec<Line<'static>> {
        let width = usize::from(width).max(1);
        let hint = strings.get("tui.switcher_hint");
        let mut lines = vec![Line::from(Span::styled(truncate(hint, width), theme.card))];
        // The hint and `+new` take a row each; the window follows the
        // highlight, keeping the last sessions in view on `+new`.
        let shown = rows.saturating_sub(2).max(1);
        let anchor = self.selected.min(self.rows.len().saturating_sub(1));
        let first = (anchor + 1).saturating_sub(shown);
        for (n, row) in self.rows.iter().enumerate().skip(first).take(shown) {
            let mark = if row.shown { '›' } else { ' ' };
            let title = row
                .title
                .as_deref()
                .unwrap_or_else(|| strings.get("tui.untitled"));
            let mut text = format!("{mark} {}  {title}", row.id);
            if row.busy {
                text.push_str("  ");
                text.push_str(strings.get("tui.running"));
            }
            lines.push(self.row(n, &text, width, theme));
        }
        let new = match &self.draft {
            Some(draft) => {
                let model = draft
                    .model
                    .as_deref()
                    .unwrap_or_else(|| strings.get("tui.default_model"));
                pmt_i18n::t!(strings, "tui.new_draft", 0 = model, 1 = draft.prompt)
            }
            None => pmt_i18n::t!(strings, "tui.new_session"),
        };
        lines.push(self.row(self.rows.len(), &new, width, theme));
        lines
    }

    fn row(&self, n: usize, text: &str, width: usize, theme: &Theme) -> Line<'static> {
        let style = if n == self.selected {
            theme.selected
        } else {
            theme.frame
        };
        Line::from(Span::styled(truncate(text, width), style))
    }
}

#[cfg(test)]
mod tests {
    use super::{Row, Switcher};

    fn row(id: &str, shown: bool) -> Row {
        Row {
            id: id.into(),
            title: None,
            busy: false,
            shown,
        }
    }

    #[test]
    fn the_shown_session_is_highlighted_and_new_is_last() {
        let mut switcher = Switcher::new(vec![row("ses_a", false), row("ses_b", true)]);
        assert_eq!(switcher.current().unwrap().id, "ses_b");
        switcher.move_selection(5);
        assert!(switcher.on_new());
        switcher.type_char('h');
        switcher.type_char('i');
        let models = ["mock/a".to_owned(), "mock/b".to_owned()];
        switcher.next_model(&models);
        switcher.next_model(&models);
        let draft = switcher.draft.clone().unwrap();
        assert_eq!(
            (draft.prompt.as_str(), draft.model.as_deref()),
            ("hi", Some("mock/b"))
        );
        switcher.move_selection(-1);
        assert_eq!(switcher.draft, None, "leaving +new drops the draft");
    }
}
