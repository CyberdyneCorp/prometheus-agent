//! The split panes (spec: tui, Split panes for subagents and tool
//! output): a subagents pane listing the session's delegated children
//! with their status, elapsed time, tokens and cost above the selected
//! child's live transcript, and a tool-output pane listing the session's
//! tool calls above the full output of the selected one (the transcript
//! keeps a one-line summary). A pane sits beside the transcript at 120
//! columns or more, below it from 80 to 119, and as a tab under 80.

use std::time::{Duration, Instant};

use pmt_i18n::Strings;
use ratatui::text::{Line, Span};

use crate::theme::Theme;
use crate::wrap::{truncate, wrap};

/// Which pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// The delegated children.
    Subagents,
    /// The session's tool calls.
    Tools,
}

/// How a shown pane shares the screen with the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrangement {
    /// Side by side (120 columns or more).
    Beside,
    /// Stacked, the pane below (80 to 119 columns).
    Below,
    /// One at a time, with a tab row (under 80 columns).
    Tabs,
}

/// The arrangement at `width` columns.
#[must_use]
pub const fn arrangement(width: u16) -> Arrangement {
    if width >= 120 {
        Arrangement::Beside
    } else if width >= 80 {
        Arrangement::Below
    } else {
        Arrangement::Tabs
    }
}

/// One item of a child's live transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Streamed {
    /// Its assistant text.
    Text(String),
    /// A tool call: tool and what it works on.
    Call(String, String),
    /// A tool call's output.
    Output(String),
    /// It ended with this status.
    Ended(String),
}

/// One delegated child.
#[derive(Debug, Clone, PartialEq)]
pub struct Child {
    /// `sa-<ULID>`.
    pub id: String,
    /// Its goal.
    pub goal: String,
    /// `running` or how it ended.
    pub status: String,
    /// When this screen saw it start.
    pub started: Instant,
    /// Its wall time once it ended.
    pub took: Option<Duration>,
    /// Tokens so far (input and output).
    pub tokens: u64,
    /// Cost so far, when known.
    pub cost_usd: Option<f64>,
    /// Its transcript as streamed (`subagent.subscribe`).
    pub stream: Vec<Streamed>,
}

impl Child {
    /// A child that just started.
    #[must_use]
    pub fn new(id: &str, goal: &str, started: Instant) -> Self {
        Self {
            id: id.to_owned(),
            goal: goal.to_owned(),
            status: "running".to_owned(),
            started,
            took: None,
            tokens: 0,
            cost_usd: None,
            stream: Vec::new(),
        }
    }

    fn row(&self, n: usize, now: Instant) -> String {
        let elapsed = self
            .took
            .unwrap_or_else(|| now.saturating_duration_since(self.started));
        let cost = self
            .cost_usd
            .map(|c| format!(" ${c:.4}"))
            .unwrap_or_default();
        format!(
            "{n}. {} · {} · {}s · {} tok{cost}",
            self.goal,
            self.status,
            elapsed.as_secs(),
            self.tokens
        )
    }
}

/// One tool call of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The model's call id.
    pub id: String,
    /// The tool.
    pub tool: String,
    /// What it works on.
    pub hint: String,
    /// Its full output, once it returned.
    pub output: Option<String>,
}

/// Both panes' contents and which one is shown.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Panes {
    /// The shown pane.
    pub shown: Option<Which>,
    /// It takes `Up`, `Down` and `Esc`.
    pub focused: bool,
    /// The session's children, oldest first.
    pub children: Vec<Child>,
    /// The selected child.
    pub child: usize,
    /// The session's tool calls, oldest first.
    pub calls: Vec<Call>,
    /// The selected call.
    pub call: usize,
}

impl Panes {
    /// Shows `which` (focused), or hides it when it is shown.
    pub fn toggle(&mut self, which: Which) {
        self.shown = (self.shown != Some(which)).then_some(which);
        self.focused = self.shown.is_some();
    }

    /// Shows `which` (`None` hides the panes).
    pub fn show(&mut self, which: Option<Which>) {
        self.shown = which;
        self.focused = which.is_some();
    }

    /// Moves the shown pane's selection `by` rows.
    pub fn move_selection(&mut self, by: isize) {
        let (selected, len) = match self.shown {
            Some(Which::Subagents) => (&mut self.child, self.children.len()),
            Some(Which::Tools) => (&mut self.call, self.calls.len()),
            None => return,
        };
        *selected = selected
            .saturating_add_signed(by)
            .min(len.saturating_sub(1));
    }

    /// The selected child.
    #[must_use]
    pub fn selected_child(&self) -> Option<&Child> {
        self.children.get(self.child)
    }

    /// The child `id`, if this screen saw it start.
    pub fn child_mut(&mut self, id: &str) -> Option<&mut Child> {
        self.children.iter_mut().find(|c| c.id == id)
    }

    /// The call `id`.
    pub fn call_mut(&mut self, id: &str) -> Option<&mut Call> {
        self.calls.iter_mut().find(|c| c.id == id)
    }

    /// The shown pane's lines for `width` by `height` cells, at `now`.
    #[must_use]
    pub fn lines(
        &self,
        width: u16,
        height: u16,
        theme: &Theme,
        strings: &Strings,
        now: Instant,
    ) -> Vec<Line<'static>> {
        let width = usize::from(width).max(1);
        let (title, rows, selected, detail) = match self.shown {
            Some(Which::Subagents) => (
                strings.get("tui.pane_subagents"),
                self.children
                    .iter()
                    .enumerate()
                    .map(|(n, c)| c.row(n + 1, now))
                    .collect::<Vec<_>>(),
                self.child,
                self.selected_child().map(stream_text).unwrap_or_default(),
            ),
            Some(Which::Tools) => (
                strings.get("tui.pane_tools"),
                self.calls
                    .iter()
                    .map(|c| format!("{} {}", c.tool, c.hint))
                    .collect(),
                self.call,
                self.calls
                    .get(self.call)
                    .and_then(|c| c.output.clone())
                    .unwrap_or_default(),
            ),
            None => return Vec::new(),
        };
        let mut lines = vec![Line::from(Span::styled(
            truncate(&format!("{title} ({})", rows.len()), width),
            theme.hunk,
        ))];
        if rows.is_empty() {
            lines.push(Line::from(Span::styled(
                strings.get("tui.pane_empty").to_owned(),
                theme.frame,
            )));
        }
        for (n, row) in rows.iter().enumerate() {
            let mark = if n == selected { "› " } else { "  " };
            let style = if n == selected && self.focused {
                theme.selected
            } else {
                theme.frame
            };
            lines.push(Line::from(Span::styled(
                truncate(&format!("{mark}{row}"), width),
                style,
            )));
        }
        lines.push(Line::from(Span::styled("─".repeat(width), theme.frame)));
        let room = usize::from(height).saturating_sub(lines.len());
        let detail: Vec<String> = wrap(&detail, width);
        // The newest lines of a long transcript or output stay in view.
        let skip = detail.len().saturating_sub(room);
        lines.extend(detail.into_iter().skip(skip).map(Line::from));
        lines
    }
}

/// A child's transcript as the pane shows it.
fn stream_text(child: &Child) -> String {
    child
        .stream
        .iter()
        .map(|item| match item {
            Streamed::Text(text) => text.clone(),
            Streamed::Call(tool, hint) => format!("· {tool} {hint}"),
            Streamed::Output(output) => output
                .lines()
                .map(|l| format!("  {l}"))
                .collect::<Vec<_>>()
                .join("\n"),
            Streamed::Ended(status) => format!("■ {status}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Arrangement, Child, Panes, Which, arrangement};

    #[test]
    fn panes_sit_beside_from_120_columns_below_from_80_and_tab_under_80() {
        assert_eq!(arrangement(120), Arrangement::Beside);
        assert_eq!(arrangement(119), Arrangement::Below);
        assert_eq!(arrangement(80), Arrangement::Below);
        assert_eq!(arrangement(79), Arrangement::Tabs);
    }

    #[test]
    fn the_same_key_hides_its_pane_and_the_other_switches() {
        let mut panes = Panes::default();
        panes.toggle(Which::Subagents);
        assert_eq!(panes.shown, Some(Which::Subagents));
        panes.toggle(Which::Tools);
        assert_eq!(panes.shown, Some(Which::Tools));
        panes.toggle(Which::Tools);
        assert_eq!(panes.shown, None);
        assert!(!panes.focused);
    }

    #[test]
    fn a_child_row_shows_status_elapsed_tokens_and_cost() {
        let start = Instant::now();
        let mut child = Child::new("sa-1", "count files", start);
        child.tokens = 1200;
        child.cost_usd = Some(0.0021);
        let row = child.row(2, start + Duration::from_secs(12));
        assert_eq!(row, "2. count files · running · 12s · 1200 tok $0.0021");
    }
}
