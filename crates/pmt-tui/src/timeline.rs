//! The session timeline (spec: tui, Session timeline scrubber): an
//! overlay listing the session's turns with their time, the first line of
//! their input, their tool calls and cost. Moving the cursor previews the
//! turn read-only; `Enter` rewinds to it (a new branch session).

use pmt_i18n::Strings;
use ratatui::text::{Line, Span};

use crate::theme::Theme;
use crate::wrap::truncate;

/// One turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// Its number, from 1.
    pub number: u32,
    /// When it started (`HH:MM:SS`).
    pub at: String,
    /// Its input's first line.
    pub first_line: String,
    /// Tool calls it made.
    pub tools: u32,
    /// What it cost, when known.
    pub cost_usd: Option<f64>,
}

/// The open timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    /// The turns, oldest first.
    pub turns: Vec<Turn>,
    /// The highlighted turn.
    pub selected: usize,
}

impl Timeline {
    /// The timeline over `turns`, the newest highlighted.
    #[must_use]
    pub fn new(turns: Vec<Turn>) -> Self {
        let selected = turns.len().saturating_sub(1);
        Self { turns, selected }
    }

    /// Moves the highlight `by` turns.
    pub fn move_selection(&mut self, by: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(by)
            .min(self.turns.len().saturating_sub(1));
    }

    /// The highlighted turn.
    #[must_use]
    pub fn selected(&self) -> Option<&Turn> {
        self.turns.get(self.selected)
    }

    /// At most `rows` lines, the highlighted turn kept in view.
    #[must_use]
    pub fn lines(
        &self,
        width: u16,
        rows: usize,
        theme: &Theme,
        strings: &Strings,
    ) -> Vec<Line<'static>> {
        let width = usize::from(width).max(1);
        let mut lines = vec![Line::from(Span::styled(
            truncate(strings.get("tui.timeline_title"), width),
            theme.card,
        ))];
        let room = rows.saturating_sub(1).max(1);
        let first = (self.selected + 1).saturating_sub(room);
        for (n, turn) in self.turns.iter().enumerate().skip(first).take(room) {
            let cost = turn
                .cost_usd
                .map_or_else(|| "-".to_owned(), |c| format!("${c:.4}"));
            let text = format!(
                "{:>3}  {}  {}  {} tools  {cost}",
                turn.number, turn.at, turn.first_line, turn.tools
            );
            let style = if n == self.selected {
                theme.selected
            } else {
                theme.frame
            };
            lines.push(Line::from(Span::styled(truncate(&text, width), style)));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::{Timeline, Turn};

    fn turn(number: u32) -> Turn {
        Turn {
            number,
            at: "10:00:00".into(),
            first_line: format!("step {number}"),
            tools: 1,
            cost_usd: None,
        }
    }

    #[test]
    fn the_newest_turn_is_highlighted_and_the_cursor_stays_in_range() {
        let mut timeline = Timeline::new((1..=12).map(turn).collect());
        assert_eq!(timeline.selected().unwrap().number, 12);
        timeline.move_selection(-5);
        assert_eq!(timeline.selected().unwrap().number, 7);
        timeline.move_selection(40);
        assert_eq!(timeline.selected().unwrap().number, 12);
        timeline.move_selection(-40);
        assert_eq!(timeline.selected().unwrap().number, 1);
    }
}
