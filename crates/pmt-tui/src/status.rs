//! The status line (spec: tui, Status line and accessibility): profile,
//! model, context-window use as a percentage, the session's cost in the
//! ledger currency and the pending approvals, with a busy glyph while a
//! turn runs (static under `ui.tui.reduced_motion`). Items that do not fit
//! a narrow terminal are dropped from the end.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr as _;

use crate::theme::Theme;
use crate::wrap::truncate;

/// The spinner frames (all one column wide, so the line never shifts).
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// The busy glyph without motion.
const STILL: &str = "●";

/// What the status line shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    /// The profile.
    pub profile: String,
    /// The session's model (`provider/model`).
    pub model: Option<String>,
    /// Context tokens in use.
    pub context_tokens: Option<u64>,
    /// The model's context window.
    pub context_window: Option<u64>,
    /// The session's cost in USD.
    pub cost_usd: Option<f64>,
    /// Approvals waiting for an answer.
    pub approvals: usize,
    /// A turn runs.
    pub busy: bool,
    /// No animation.
    pub reduced_motion: bool,
    /// Animation frame counter.
    pub frame: usize,
    /// A connection note (`reconnecting`, `gateway restarting`).
    pub note: Option<String>,
}

impl Status {
    /// Context use as a whole percentage, when the window is known.
    #[must_use]
    pub fn context_percent(&self) -> Option<u64> {
        let (used, window) = (self.context_tokens?, self.context_window?);
        (window > 0).then(|| (used.saturating_mul(100) / window).min(100))
    }

    fn items(&self) -> Vec<String> {
        let mut items = vec![self.profile.clone()];
        if let Some(model) = &self.model {
            items.push(model.clone());
        }
        if let Some(percent) = self.context_percent() {
            items.push(format!("ctx {percent}%"));
        }
        if let Some(cost) = self.cost_usd {
            items.push(format!("${cost:.4}"));
        }
        if self.approvals > 0 {
            let s = if self.approvals == 1 { "" } else { "s" };
            items.push(format!("{} approval{s}", self.approvals));
        }
        if let Some(note) = &self.note {
            items.push(note.clone());
        }
        items
    }

    /// The glyph shown while busy.
    #[must_use]
    pub fn busy_glyph(&self) -> &'static str {
        if self.reduced_motion {
            STILL
        } else {
            SPINNER[self.frame % SPINNER.len()]
        }
    }

    /// The status line for `width` columns.
    #[must_use]
    pub fn line(&self, width: u16, theme: &Theme) -> Line<'static> {
        let width = usize::from(width);
        let lead = if self.busy {
            format!("{} ", self.busy_glyph())
        } else {
            "  ".to_owned()
        };
        let room = width.saturating_sub(lead.width());
        let mut text = String::new();
        for item in self.items() {
            let next = if text.is_empty() {
                item
            } else {
                format!("{text} │ {item}")
            };
            if next.width() > room && !text.is_empty() {
                break;
            }
            text = next;
        }
        let text = truncate(&text, room);
        let pad = " ".repeat(room.saturating_sub(text.width()));
        Line::from(vec![
            Span::styled(lead, theme.status.patch(theme.busy)),
            Span::styled(format!("{text}{pad}"), theme.status),
        ])
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::TuiTheme;

    use super::Status;
    use crate::theme::Theme;

    fn text(status: &Status, width: u16) -> String {
        let line = status.line(width, &Theme::new(TuiTheme::Dark, true));
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn full() -> Status {
        Status {
            profile: "work".into(),
            model: Some("mock/m1".into()),
            context_tokens: Some(1500),
            context_window: Some(8000),
            cost_usd: Some(0.0123),
            approvals: 2,
            ..Status::default()
        }
    }

    #[test]
    fn every_item_shows_when_there_is_room() {
        assert_eq!(
            text(&full(), 70).trim_end(),
            "  work │ mock/m1 │ ctx 18% │ $0.0123 │ 2 approvals"
        );
    }

    #[test]
    fn a_narrow_line_drops_items_from_the_end() {
        assert_eq!(text(&full(), 30).trim_end(), "  work │ mock/m1 │ ctx 18%");
        assert_eq!(text(&full(), 30).chars().count(), 30, "padded to the width");
    }

    #[test]
    fn reduced_motion_shows_a_still_glyph() {
        let mut status = full();
        status.busy = true;
        status.frame = 3;
        assert!(text(&status, 70).starts_with("⠸ "));
        status.reduced_motion = true;
        assert!(text(&status, 70).starts_with("● "));
    }
}
