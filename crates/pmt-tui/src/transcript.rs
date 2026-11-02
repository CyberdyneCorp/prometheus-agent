//! The conversation as the TUI shows it: one entry per message, tool call,
//! notice or error, wrapped to the screen's width when drawn.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr as _;

use crate::theme::Theme;
use crate::wrap::wrap;

/// What an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The person's prompt.
    User,
    /// The assistant's text.
    Assistant,
    /// A tool call in progress.
    Tool,
    /// A command's output or a status message.
    Notice,
    /// An error.
    Error,
}

impl Kind {
    const fn prefix(self) -> &'static str {
        match self {
            Self::User => "› ",
            Self::Assistant => "",
            Self::Tool => "· ",
            Self::Notice => "  ",
            Self::Error => "! ",
        }
    }
}

/// One shown item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What it is.
    pub kind: Kind,
    /// Its text.
    pub text: String,
}

/// Every entry, oldest first.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    entries: Vec<Entry>,
}

impl Transcript {
    /// Adds an entry.
    pub fn push(&mut self, kind: Kind, text: impl Into<String>) {
        self.entries.push(Entry {
            kind,
            text: text.into(),
        });
    }

    /// Forgets every entry (`/clear`, a transcript reload).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The entries, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The text of the newest assistant entry (`/copy`).
    #[must_use]
    pub fn last_answer(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.kind == Kind::Assistant)
            .map(|e| e.text.as_str())
    }

    /// Every entry wrapped to `width`, a blank line between messages.
    #[must_use]
    pub fn lines(&self, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (n, entry) in self.entries.iter().enumerate() {
            let starts_turn = matches!(entry.kind, Kind::User | Kind::Assistant);
            if n > 0 && starts_turn {
                lines.push(Line::default());
            }
            let prefix = entry.kind.prefix();
            let indent = " ".repeat(prefix.width());
            let style = theme.entry(entry.kind);
            let room = usize::from(width).saturating_sub(prefix.width());
            for (row, text) in wrap(&entry.text, room).into_iter().enumerate() {
                let lead = if row == 0 { prefix } else { indent.as_str() };
                lines.push(Line::from(vec![
                    Span::styled(lead.to_owned(), style),
                    Span::styled(text, style),
                ]));
            }
        }
        lines
    }
}
