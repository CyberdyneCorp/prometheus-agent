//! Colours (spec: tui, Status line and accessibility): a dark and a light
//! theme for `ui.tui.theme`, and no colour at all under `NO_COLOR`.

use pmt_config::schema::TuiTheme;
use ratatui::style::{Color, Modifier, Style};

use crate::transcript::Kind;

/// The styles of every part of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    user: Style,
    assistant: Style,
    tool: Style,
    notice: Style,
    error: Style,
    /// The status line.
    pub status: Style,
    /// A card asking for an answer (approval, question).
    pub card: Style,
    /// The composer's frame.
    pub frame: Style,
    /// The busy glyph.
    pub busy: Style,
}

impl Theme {
    /// The theme `ui.tui.theme` names; `no_color` (`NO_COLOR` set or
    /// `--no-color`) keeps only bold and reverse video. `auto` is dark
    /// until the terminal's background is detected.
    #[must_use]
    pub fn new(theme: TuiTheme, no_color: bool) -> Self {
        if no_color {
            return Self::mono();
        }
        match theme {
            TuiTheme::Light => Self::light(),
            TuiTheme::Dark | TuiTheme::Auto => Self::dark(),
        }
    }

    fn dark() -> Self {
        Self {
            user: Style::new().fg(Color::Cyan),
            assistant: Style::new(),
            tool: Style::new().fg(Color::DarkGray),
            notice: Style::new().fg(Color::Yellow),
            error: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            status: Style::new().fg(Color::Black).bg(Color::Gray),
            card: Style::new().fg(Color::Yellow),
            frame: Style::new().fg(Color::DarkGray),
            busy: Style::new().fg(Color::Magenta),
        }
    }

    fn light() -> Self {
        Self {
            user: Style::new().fg(Color::Blue),
            assistant: Style::new().fg(Color::Black),
            tool: Style::new().fg(Color::Gray),
            notice: Style::new().fg(Color::Magenta),
            error: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            status: Style::new().fg(Color::White).bg(Color::DarkGray),
            card: Style::new().fg(Color::Magenta),
            frame: Style::new().fg(Color::Gray),
            busy: Style::new().fg(Color::Blue),
        }
    }

    fn mono() -> Self {
        let plain = Style::new();
        Self {
            user: plain.add_modifier(Modifier::BOLD),
            assistant: plain,
            tool: plain,
            notice: plain,
            error: plain.add_modifier(Modifier::BOLD),
            status: plain.add_modifier(Modifier::REVERSED),
            card: plain.add_modifier(Modifier::BOLD),
            frame: plain,
            busy: plain,
        }
    }

    /// The style of a transcript entry.
    #[must_use]
    pub const fn entry(&self, kind: Kind) -> Style {
        match kind {
            Kind::User => self.user,
            Kind::Assistant => self.assistant,
            Kind::Tool => self.tool,
            Kind::Notice => self.notice,
            Kind::Error => self.error,
        }
    }

    /// Whether any colour is used.
    #[must_use]
    pub fn colored(&self) -> bool {
        *self != Self::mono()
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::TuiTheme;

    use super::Theme;

    #[test]
    fn no_color_wins_over_any_theme() {
        for theme in [TuiTheme::Auto, TuiTheme::Dark, TuiTheme::Light] {
            assert!(!Theme::new(theme, true).colored());
            assert!(Theme::new(theme, false).colored());
        }
        assert_ne!(
            Theme::new(TuiTheme::Dark, false),
            Theme::new(TuiTheme::Light, false)
        );
    }
}
