//! The pet and achievement toasts on the TUI's screen (spec: pets,
//! Terminal pet rendering; Achievement unlocks celebrate). The pet sits in
//! the bottom-right corner of the transcript: half-block cells are painted
//! into the screen buffer; a graphics-protocol pet only reserves its box,
//! which the host fills with the image after each frame. A toast is a
//! dismissible box in the top-right corner.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::theme::Theme;
use crate::wrap::truncate;

/// One half-block cell: the top and bottom pixel (RGBA).
pub type Cell = ([u8; 4], [u8; 4]);

/// What the pet looks like this frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PetView {
    /// Half-block cells, row by row (empty for a graphics pet).
    pub cells: Vec<Vec<Cell>>,
    /// A graphics pet's box in cells (columns, rows): reserved blank.
    pub graphics: Option<(u16, u16)>,
    /// The affection reaction plays.
    pub heart: bool,
}

/// An achievement toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// Its first line.
    pub title: String,
    /// What was reached.
    pub body: String,
}

const OPAQUE: u8 = 32;

fn colour(p: [u8; 4]) -> Color {
    Color::Rgb(p[0], p[1], p[2])
}

impl PetView {
    /// The box in cells.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        self.graphics.unwrap_or_else(|| {
            let rows = u16::try_from(self.cells.len()).unwrap_or(u16::MAX);
            let cols = self.cells.first().map_or(0, Vec::len);
            (u16::try_from(cols).unwrap_or(u16::MAX), rows)
        })
    }

    /// Draws the pet in the bottom-right corner of `area`; where it
    /// landed (`None`: it does not fit and is left out).
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) -> Option<Rect> {
        let (cols, rows) = self.size();
        // The pet takes at most half of the transcript's width and leaves
        // its top two rows to the conversation.
        if cols == 0 || rows == 0 || cols + 2 > area.width / 2 || rows + 2 > area.height {
            return None;
        }
        let at = Rect::new(area.right() - cols, area.bottom() - rows, cols, rows);
        frame.render_widget(Clear, at);
        paint(frame.buffer_mut(), at, &self.cells);
        if self.heart {
            let heart = Rect::new(at.x.saturating_sub(2), at.y, 1, 1);
            frame.render_widget(
                Paragraph::new(Span::styled("♥", Style::new().fg(Color::Red))),
                heart,
            );
        }
        Some(at)
    }
}

fn paint(buffer: &mut Buffer, at: Rect, cells: &[Vec<Cell>]) {
    for (y, row) in (at.y..at.bottom()).zip(cells) {
        for (x, &(top, bottom)) in (at.x..at.right()).zip(row) {
            let cell = &mut buffer[(x, y)];
            match (top[3] >= OPAQUE, bottom[3] >= OPAQUE) {
                (false, false) => {
                    cell.set_symbol(" ");
                }
                (true, false) => {
                    cell.set_symbol("▀").set_fg(colour(top));
                }
                (false, true) => {
                    cell.set_symbol("▄").set_fg(colour(bottom));
                }
                (true, true) => {
                    cell.set_symbol("▀")
                        .set_fg(colour(top))
                        .set_bg(colour(bottom));
                }
            }
        }
    }
}

impl Toast {
    /// Draws the toast in the top-right corner of `area`.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme, hint: &str) {
        let width = [self.title.as_str(), self.body.as_str(), hint]
            .into_iter()
            .map(unicode_width::UnicodeWidthStr::width)
            .max()
            .unwrap_or(0)
            .saturating_add(4);
        let width = u16::try_from(width).unwrap_or(u16::MAX).min(area.width);
        if width < 12 || area.height < 5 {
            return;
        }
        let at = Rect::new(area.right() - width, area.y, width, 5);
        let inner = usize::from(width.saturating_sub(4));
        let lines = vec![
            Line::from(Span::styled(
                truncate(&self.title, inner),
                theme.card.add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(truncate(&self.body, inner), theme.card)),
            Line::from(Span::styled(truncate(hint, inner), theme.frame)),
        ];
        frame.render_widget(Clear, at);
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::new().borders(Borders::ALL).border_style(theme.frame)),
            at,
        );
    }
}
