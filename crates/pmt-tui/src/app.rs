//! The screen: the startup banner and the transcript, an optional card
//! asking for an answer, an overlay (palette, session switcher) or the
//! completion list, the composer and the status line, laid out to stay
//! usable down to 40 columns by 12 rows (spec: tui, Status line and
//! accessibility): the composer and the newest transcript line always
//! keep a row each. Rendering also records where the clickable parts
//! (banner headers, the `<n> live` status item) landed, for the mouse.

use std::cell::RefCell;
use std::sync::Arc;

use pmt_i18n::{Strings, Surface};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr as _;

use crate::banner::Banner;
use crate::composer::Composer;
use crate::palette::Palette;
use crate::status::Status;
use crate::switcher::Switcher;
use crate::theme::Theme;
use crate::transcript::Transcript;
use crate::wrap::{truncate, wrap};

/// Composer rows shown at most.
const COMPOSER_ROWS: usize = 6;
/// Completion rows shown at most.
const COMPLETION_ROWS: usize = 6;
/// The composer's prompt.
const PROMPT: &str = "› ";

/// An overlay above the composer that takes the keys while open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// The command palette.
    Palette(Palette),
    /// The live session switcher.
    Switcher(Switcher),
}

/// Overlay rows shown at most.
const OVERLAY_ROWS: usize = 10;

/// What a click on the screen hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// A banner section's header.
    BannerSection(usize),
    /// The `<n> live` status item.
    Live,
}

/// A question waiting for the person (an approval, a clarify question).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// Its first line.
    pub title: String,
    /// The details.
    pub body: String,
    /// How to answer.
    pub hint: String,
}

/// Everything on the screen.
#[derive(Debug, Clone)]
pub struct App {
    /// The conversation.
    pub transcript: Transcript,
    /// What is being typed.
    pub composer: Composer,
    /// The status line.
    pub status: Status,
    /// A question waiting for an answer.
    pub card: Option<Card>,
    /// Completion candidates for the word at the cursor.
    pub completions: Vec<String>,
    /// Lines scrolled back from the newest.
    pub scroll: usize,
    /// The colours.
    pub theme: Theme,
    /// The startup banner, above the transcript.
    pub banner: Option<Banner>,
    /// The open overlay.
    pub overlay: Option<Overlay>,
    /// The status line is shown (`ui.status_bar`).
    pub status_visible: bool,
    /// The profile's TUI strings (spec: i18n).
    pub strings: Arc<Strings>,
    /// Where the last frame drew clickable parts: `(row, columns, hit)`.
    hits: RefCell<Vec<(u16, std::ops::Range<u16>, Hit)>>,
}

impl App {
    /// An empty screen.
    #[must_use]
    pub fn new(theme: Theme, status: Status) -> Self {
        Self {
            transcript: Transcript::default(),
            composer: Composer::default(),
            status,
            card: None,
            completions: Vec::new(),
            scroll: 0,
            theme,
            banner: None,
            overlay: None,
            status_visible: true,
            strings: Strings::english(Surface::Tui),
            hits: RefCell::new(Vec::new()),
        }
    }

    /// What the last frame drew at (`column`, `row`).
    #[must_use]
    pub fn hit(&self, column: u16, row: u16) -> Option<Hit> {
        self.hits
            .borrow()
            .iter()
            .find(|(r, columns, _)| *r == row && columns.contains(&column))
            .map(|(_, _, hit)| *hit)
    }

    /// Scrolls `lines` back (older).
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_add(lines);
    }

    /// Scrolls `lines` forward (newer).
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    /// Draws the screen.
    pub fn render(&self, frame: &mut Frame<'_>) {
        self.hits.borrow_mut().clear();
        let area = frame.area();
        let status_rows = u16::from(self.status_visible);
        let composer_rows = composer_rows(&self.composer, area.height);
        let fixed = composer_rows + status_rows + 2;
        let card_rows = self.card_lines(area.width).len();
        let card_rows = u16::try_from(card_rows)
            .unwrap_or(u16::MAX)
            .min(area.height.saturating_sub(fixed) / 2);
        let below = self.below_rows(area.width);
        let below_rows = u16::try_from(below)
            .unwrap_or(0)
            .min(area.height.saturating_sub(fixed + card_rows));
        let [transcript, card, below, composer, status] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(card_rows),
            Constraint::Length(below_rows),
            Constraint::Length(composer_rows),
            Constraint::Length(status_rows),
        ])
        .areas(area);
        self.render_transcript(frame, transcript);
        if card_rows > 0 {
            let lines = self.card_lines(card.width);
            frame.render_widget(Paragraph::new(lines), card);
        }
        if below_rows > 0 {
            self.render_below(frame, below);
        }
        self.render_composer(frame, composer);
        if status_rows > 0 {
            frame.render_widget(
                Paragraph::new(self.status.line(status.width, &self.theme, &self.strings)),
                status,
            );
            if let Some(columns) = self.status.live_columns(status.width, &self.strings) {
                self.hits.borrow_mut().push((status.y, columns, Hit::Live));
            }
        }
    }

    /// Rows of the overlay, else of the completion list.
    fn below_rows(&self, width: u16) -> usize {
        match &self.overlay {
            Some(Overlay::Palette(p)) => p
                .lines(width, OVERLAY_ROWS, &self.theme, &self.strings)
                .len(),
            Some(Overlay::Switcher(s)) => s
                .lines(width, OVERLAY_ROWS, &self.theme, &self.strings)
                .len(),
            None => self.completions.len().min(COMPLETION_ROWS),
        }
    }

    fn render_below(&self, frame: &mut Frame<'_>, area: Rect) {
        let rows = usize::from(area.height);
        let lines = match &self.overlay {
            Some(Overlay::Palette(p)) => p.lines(area.width, rows, &self.theme, &self.strings),
            Some(Overlay::Switcher(s)) => s.lines(area.width, rows, &self.theme, &self.strings),
            None => return self.render_completions(frame, area),
        };
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_transcript(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines: Vec<(Line<'static>, Option<usize>)> = self
            .banner
            .as_ref()
            .map(|b| b.lines(area.width, &self.theme, &self.strings))
            .unwrap_or_default();
        if !lines.is_empty() && !self.transcript.entries().is_empty() {
            lines.push((Line::default(), None));
        }
        lines.extend(
            self.transcript
                .lines(area.width, &self.theme)
                .into_iter()
                .map(|l| (l, None)),
        );
        let height = usize::from(area.height);
        let scroll = self.scroll.min(lines.len().saturating_sub(height));
        let end = lines.len() - scroll;
        let start = end.saturating_sub(height);
        let mut hits = self.hits.borrow_mut();
        for (row, (_, section)) in lines[start..end].iter().enumerate() {
            if let (Some(section), Ok(row)) = (section, u16::try_from(row)) {
                hits.push((
                    area.y + row,
                    area.x..area.right(),
                    Hit::BannerSection(*section),
                ));
            }
        }
        let shown: Vec<Line<'static>> = lines.drain(start..end).map(|(l, _)| l).collect();
        frame.render_widget(Paragraph::new(shown), area);
    }

    fn card_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(card) = &self.card else {
            return Vec::new();
        };
        let width = usize::from(width).max(1);
        let style = self.theme.card;
        let mut lines = vec![Line::from(Span::styled(
            truncate(&format!("? {}", card.title), width),
            style,
        ))];
        for text in wrap(&card.body, width.saturating_sub(2)) {
            lines.push(Line::from(Span::styled(format!("  {text}"), style)));
        }
        lines.push(Line::from(Span::styled(truncate(&card.hint, width), style)));
        lines
    }

    fn render_completions(&self, frame: &mut Frame<'_>, area: Rect) {
        let width = usize::from(area.width);
        let lines: Vec<Line<'static>> = self
            .completions
            .iter()
            .take(usize::from(area.height))
            .map(|item| Line::from(Span::styled(truncate(item, width), self.theme.frame)))
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_composer(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(self.theme.frame);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let (row, column) = self.composer.cursor_position();
        let rows = usize::from(inner.height).max(1);
        let first = (row + 1).saturating_sub(rows);
        let room = usize::from(inner.width).saturating_sub(PROMPT.width());
        // The cursor's column stays visible on a long line.
        let shift = (column + 1).saturating_sub(room);
        let text = self.composer.text();
        let lines: Vec<Line<'static>> = text
            .split('\n')
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(n, line)| {
                let lead = if n == 0 { PROMPT } else { "  " };
                let shown: String = if n == row {
                    skip_columns(line, shift)
                } else {
                    line.to_owned()
                };
                Line::from(vec![Span::raw(lead), Span::raw(truncate(&shown, room))])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        let x = inner.x + u16::try_from(PROMPT.width() + column - shift).unwrap_or(0);
        let y = inner.y + u16::try_from(row - first).unwrap_or(0);
        frame.set_cursor_position(Position::new(x.min(inner.right().saturating_sub(1)), y));
    }
}

/// `line` without its first `columns` display columns.
fn skip_columns(line: &str, columns: usize) -> String {
    let mut skipped = 0;
    line.chars()
        .skip_while(|c| {
            let w = unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0);
            let skip = skipped < columns;
            if skip {
                skipped += w;
            }
            skip
        })
        .collect()
}

/// The composer's rows (its top border included): its lines, at most
/// [`COMPOSER_ROWS`], and fewer on a short screen.
fn composer_rows(composer: &Composer, height: u16) -> u16 {
    let lines = composer.text().split('\n').count().clamp(1, COMPOSER_ROWS);
    let cap = (usize::from(height) / 4).max(1);
    u16::try_from(lines.min(cap)).unwrap_or(1) + 1
}
