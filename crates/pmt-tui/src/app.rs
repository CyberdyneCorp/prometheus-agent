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
use crate::diff::DiffHunk;
use crate::palette::Palette;
use crate::panes::{Arrangement, Panes, Which, arrangement};
use crate::status::Status;
use crate::switcher::Switcher;
use crate::theme::Theme;
use crate::timeline::Timeline;
use crate::transcript::Transcript;
use crate::wrap::{truncate, wrap};

/// Composer rows shown at most.
const COMPOSER_ROWS: usize = 6;
/// Completion rows shown at most.
const COMPLETION_ROWS: usize = 6;
/// The composer's prompt unless the theme brands it.
pub const PROMPT: &str = "› ";

/// An overlay above the composer that takes the keys while open.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    /// The command palette.
    Palette(Palette),
    /// The live session switcher.
    Switcher(Switcher),
    /// The session timeline.
    Timeline(Timeline),
    /// The language picker.
    Languages(crate::languages::LanguagePicker),
    /// The pet picker (`/pet list`).
    Pets(crate::pet_picker::PetPicker),
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Card {
    /// Its first line.
    pub title: String,
    /// The details.
    pub body: String,
    /// A file edit's numbered hunks, after the details.
    pub diff: Vec<DiffHunk>,
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
    /// The composer's prompt (a theme's `branding.prompt_symbol`).
    pub prompt: String,
    /// The profile's TUI strings (spec: i18n).
    pub strings: Arc<Strings>,
    /// The split panes.
    pub panes: Panes,
    /// A turn shown read-only in place of the transcript (the timeline's
    /// preview): its heading and entries.
    pub preview: Option<(String, Transcript)>,
    /// The pet in the transcript's corner (spec: pets).
    pub pet: Option<crate::pet::PetView>,
    /// An achievement toast, until dismissed.
    pub toast: Option<crate::pet::Toast>,
    /// Where the last frame drew clickable parts: `(row, columns, hit)`.
    hits: RefCell<Vec<(u16, std::ops::Range<u16>, Hit)>>,
    /// Where the last frame put the pet (a graphics pet is drawn there by
    /// the host).
    pet_at: std::cell::Cell<Option<Rect>>,
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
            panes: Panes::default(),
            preview: None,
            prompt: PROMPT.to_owned(),
            pet: None,
            toast: None,
            hits: RefCell::new(Vec::new()),
            pet_at: std::cell::Cell::new(None),
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

    /// Where the last frame put the pet's box.
    #[must_use]
    pub fn pet_area(&self) -> Option<Rect> {
        self.pet_at.get()
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
        // A diff to review may take all but a transcript row; other cards
        // half of what the fixed rows leave.
        let card_cap = if self.card.as_ref().is_some_and(|c| !c.diff.is_empty()) {
            area.height.saturating_sub(fixed + 1)
        } else {
            area.height.saturating_sub(fixed) / 2
        };
        let card_rows = self.card_lines(area.width, card_cap).len();
        let card_rows = u16::try_from(card_rows).unwrap_or(u16::MAX).min(card_cap);
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
        self.render_middle(frame, transcript);
        self.pet_at.set(
            self.pet
                .as_ref()
                .and_then(|pet| pet.render(frame, transcript)),
        );
        if let Some(toast) = &self.toast {
            toast.render(
                frame,
                transcript,
                &self.theme,
                self.strings.get("tui.toast_dismiss"),
            );
        }
        if card_rows > 0 {
            let lines = self.card_lines(card.width, card.height);
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
            Some(Overlay::Timeline(t)) => t
                .lines(width, OVERLAY_ROWS, &self.theme, &self.strings)
                .len(),
            Some(Overlay::Languages(l)) => l
                .lines(width, OVERLAY_ROWS, &self.theme, &self.strings)
                .len(),
            Some(Overlay::Pets(p)) => p
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
            Some(Overlay::Timeline(t)) => t.lines(area.width, rows, &self.theme, &self.strings),
            Some(Overlay::Languages(l)) => l.lines(area.width, rows, &self.theme, &self.strings),
            Some(Overlay::Pets(p)) => p.lines(area.width, rows, &self.theme, &self.strings),
            None => return self.render_completions(frame, area),
        };
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// The transcript, with the shown pane beside, below or as a tab.
    fn render_middle(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(which) = self.panes.shown else {
            return self.render_transcript(frame, area);
        };
        match arrangement(area.width) {
            Arrangement::Beside => {
                let [left, right] =
                    Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                        .areas(area);
                self.render_transcript(frame, left);
                self.render_pane(frame, right, Borders::LEFT);
            }
            Arrangement::Below => {
                let [top, bottom] =
                    Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .areas(area);
                self.render_transcript(frame, top);
                self.render_pane(frame, bottom, Borders::TOP);
            }
            Arrangement::Tabs => {
                let [tabs, rest] =
                    Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
                frame.render_widget(Paragraph::new(self.tab_row(which, tabs.width)), tabs);
                self.render_pane(frame, rest, Borders::NONE);
            }
        }
    }

    /// The tab row under 80 columns: the transcript and the two panes,
    /// the shown one marked.
    fn tab_row(&self, shown: Which, width: u16) -> Line<'static> {
        let tabs = [
            (None, "tui.pane_transcript"),
            (Some(Which::Subagents), "tui.pane_subagents"),
            (Some(Which::Tools), "tui.pane_tools"),
        ];
        let spans: Vec<Span<'static>> = tabs
            .iter()
            .map(|(which, key)| {
                let label = format!(" {} ", self.strings.get(key));
                if *which == Some(shown) {
                    Span::styled(label, self.theme.selected)
                } else {
                    Span::styled(label, self.theme.frame)
                }
            })
            .collect();
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        if text.width() > usize::from(width) {
            return Line::from(Span::styled(
                truncate(&text, usize::from(width)),
                self.theme.frame,
            ));
        }
        Line::from(spans)
    }

    fn render_pane(&self, frame: &mut Frame<'_>, area: Rect, borders: Borders) {
        let block = Block::new().borders(borders).border_style(self.theme.frame);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = self.panes.lines(
            inner.width,
            inner.height,
            &self.theme,
            &self.strings,
            std::time::Instant::now(),
        );
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_transcript(&self, frame: &mut Frame<'_>, area: Rect) {
        if let Some((heading, turn)) = &self.preview {
            let mut lines = vec![Line::from(Span::styled(
                truncate(heading, usize::from(area.width)),
                self.theme.card,
            ))];
            lines.extend(turn.lines(area.width, &self.theme));
            lines.truncate(usize::from(area.height));
            frame.render_widget(Paragraph::new(lines), area);
            return;
        }
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

    /// The card in at most `rows` lines: when it is longer, the end of
    /// its details gives way to `…` so the title and the hint (how to
    /// answer) always show.
    fn card_lines(&self, width: u16, rows: u16) -> Vec<Line<'static>> {
        let mut lines = self.full_card(width);
        let rows = usize::from(rows);
        if lines.len() > rows && rows >= 3 {
            let hint = lines.pop();
            lines.truncate(rows - 2);
            lines.push(Line::from(Span::styled("  …", self.theme.card)));
            lines.extend(hint);
        }
        lines
    }

    fn full_card(&self, width: u16) -> Vec<Line<'static>> {
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
        let cells = u16::try_from(width).unwrap_or(u16::MAX);
        lines.extend(crate::diff::lines(&card.diff, cells, &self.theme));
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
        let prompt = self.prompt.as_str();
        let room = usize::from(inner.width).saturating_sub(prompt.width());
        // The cursor's column stays visible on a long line.
        let shift = (column + 1).saturating_sub(room);
        let text = self.composer.text();
        let lines: Vec<Line<'static>> = text
            .split('\n')
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(n, line)| {
                let lead = if n == 0 {
                    prompt.to_owned()
                } else {
                    " ".repeat(prompt.width())
                };
                let shown: String = if n == row {
                    skip_columns(line, shift)
                } else {
                    line.to_owned()
                };
                Line::from(vec![Span::raw(lead), Span::raw(truncate(&shown, room))])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
        let x = inner.x + u16::try_from(prompt.width() + column - shift).unwrap_or(0);
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
