//! The language picker (spec: i18n, Language switchers show endonyms):
//! `/language` lists every language the profile can select by its own
//! name with its id, the current one marked; `Enter` writes the choice to
//! `display.language`, and the TUI's text follows it without a restart.

use ratatui::text::{Line, Span};

use crate::theme::Theme;
use crate::wrap::truncate;

/// One selectable language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The `display.language` id.
    pub id: String,
    /// Its own name for itself.
    pub endonym: String,
}

/// The open picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguagePicker {
    /// Every language, by id.
    pub choices: Vec<Choice>,
    /// The highlighted row.
    pub selected: usize,
    /// The profile's language now.
    pub current: String,
}

impl LanguagePicker {
    /// The picker over `choices`, the current language highlighted.
    #[must_use]
    pub fn new(choices: Vec<Choice>, current: &str) -> Self {
        let selected = choices.iter().position(|c| c.id == current).unwrap_or(0);
        Self {
            choices,
            selected,
            current: current.to_owned(),
        }
    }

    /// Moves the highlight `by` rows, staying on the list.
    pub fn move_selection(&mut self, by: isize) {
        let last = self.choices.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(by).min(last);
    }

    /// The highlighted language.
    #[must_use]
    pub fn chosen(&self) -> Option<&Choice> {
        self.choices.get(self.selected)
    }

    /// The overlay: a hint, then `<endonym> (<id>)` rows, the current
    /// language marked `›`, scrolled to keep the highlight in view.
    #[must_use]
    pub fn lines(
        &self,
        width: u16,
        rows: usize,
        theme: &Theme,
        strings: &pmt_i18n::Strings,
    ) -> Vec<Line<'static>> {
        let width = usize::from(width).max(1);
        let hint = strings.get("tui.language_hint");
        let mut lines = vec![Line::from(Span::styled(truncate(hint, width), theme.card))];
        let shown = rows.saturating_sub(1).max(1);
        let first = (self.selected + 1).saturating_sub(shown);
        for (n, choice) in self.choices.iter().enumerate().skip(first).take(shown) {
            let mark = if choice.id == self.current {
                '›'
            } else {
                ' '
            };
            let text = format!("{mark} {} ({})", choice.endonym, choice.id);
            let style = if n == self.selected {
                theme.selected
            } else {
                theme.card
            };
            lines.push(Line::from(Span::styled(truncate(&text, width), style)));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> LanguagePicker {
        let choice = |id: &str, endonym: &str| Choice {
            id: id.into(),
            endonym: endonym.into(),
        };
        LanguagePicker::new(
            vec![
                choice("ar", "العربية"),
                choice("de", "Deutsch"),
                choice("en", "English"),
            ],
            "en",
        )
    }

    #[test]
    fn rows_show_endonyms_with_ids_and_the_current_language_is_highlighted() {
        let mut picker = picker();
        assert_eq!(picker.chosen().unwrap().id, "en");
        let strings = pmt_i18n::Strings::english(pmt_i18n::Surface::Tui);
        let text: Vec<String> = picker
            .lines(
                60,
                10,
                &Theme::new(pmt_config::schema::TuiTheme::Dark, true),
                &strings,
            )
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(text[1], "  العربية (ar)");
        assert_eq!(text[3], "› English (en)");
        picker.move_selection(-5);
        assert_eq!(picker.chosen().unwrap().id, "ar");
        picker.move_selection(9);
        assert_eq!(picker.chosen().unwrap().id, "en");
    }
}
