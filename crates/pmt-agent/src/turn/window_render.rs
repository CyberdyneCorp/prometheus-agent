//! How `/context` shows a [`ContextView`]: a 5×20 cell grid on the CLI,
//! the TUI and the desktop, a text gauge on messaging platforms (spec:
//! context-compression, Context window view).

use std::fmt::Write as _;

use super::window::ContextView;

/// How the view is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// A 5×20 grid of cells, each 1% of the window (CLI, TUI, desktop).
    Grid,
    /// A one-line gauge (messaging).
    Text,
}

const ROWS: usize = 5;
const COLUMNS: usize = 20;
const CELLS: usize = ROWS * COLUMNS;

fn share(tokens: u64, window: u64) -> f64 {
    if window == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let share = tokens as f64 * 100.0 / window as f64;
    share
}

/// One glyph per cell: each used category gets its share (at least one
/// cell when it holds anything), the rest is free.
fn cells(view: &ContextView) -> Vec<char> {
    let mut cells = Vec::with_capacity(CELLS);
    for category in view.categories.iter().filter(|c| c.name != "free") {
        if category.tokens == 0 {
            continue;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (share(category.tokens, view.window).round() as usize).max(1);
        let room = CELLS - cells.len();
        cells.extend(std::iter::repeat_n(category.glyph, n.min(room)));
    }
    cells.resize(CELLS, '.');
    cells
}

/// The view as text.
#[must_use]
pub fn render(view: &ContextView, style: Style, all: bool) -> String {
    let used = view.used();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Context window: {used} of {} tokens ({:.0}%)",
        view.window,
        share(used, view.window)
    );
    match style {
        Style::Grid => {
            for row in cells(view).chunks(COLUMNS) {
                let _ = writeln!(out, "  {}", row.iter().collect::<String>());
            }
        }
        Style::Text => {
            let filled = cells(view).iter().filter(|c| **c != '.').count() / ROWS;
            let _ = writeln!(
                out,
                "{}{}",
                "█".repeat(filled),
                "░".repeat(COLUMNS - filled)
            );
        }
    }
    for c in &view.categories {
        let glyph = if style == Style::Grid {
            format!("{} ", c.glyph)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "{glyph}{}: {} tokens ({:.1}%)",
            c.name,
            c.tokens,
            share(c.tokens, view.window)
        );
    }
    let _ = writeln!(
        out,
        "Compaction trigger: {} tokens ({} headroom)",
        view.trigger_tokens,
        view.trigger_tokens.saturating_sub(used)
    );
    let last = view.last_compaction.map_or_else(
        || "never".to_owned(),
        |at| at.strftime("%Y-%m-%d %H:%M").to_string(),
    );
    let _ = writeln!(out, "Compactions: {} (last: {last})", view.compactions);
    let _ = writeln!(
        out,
        "Throughput: {} input, {} output tokens",
        view.input_tokens, view.output_tokens
    );
    files(&mut out, view);
    if all {
        details(&mut out, view);
    }
    out.trim_end().to_owned()
}

fn files(out: &mut String, view: &ContextView) {
    if view.files.is_empty() {
        let _ = writeln!(out, "Context files: none");
        return;
    }
    let _ = writeln!(out, "Context files:");
    for file in &view.files {
        let _ = writeln!(
            out,
            "  {}: {} tokens, {}",
            file.name,
            file.tokens,
            file.state.as_str()
        );
    }
}

fn details(out: &mut String, view: &ContextView) {
    let _ = writeln!(out, "Toolsets:");
    for set in &view.toolsets {
        let _ = writeln!(
            out,
            "  {}: {} tools, {} tokens",
            set.toolset, set.tools, set.tokens
        );
    }
    if !view.skills.is_empty() {
        let _ = writeln!(out, "Skills (index / full load):");
        for skill in &view.skills {
            let _ = writeln!(
                out,
                "  {}: {} / {} tokens",
                skill.name, skill.index_tokens, skill.full_tokens
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::window::Category;

    fn view() -> ContextView {
        let category = |name, glyph, tokens| Category {
            name,
            glyph,
            tokens,
        };
        ContextView {
            window: 1000,
            categories: vec![
                category("system prompt", 's', 100),
                category("conversation", 'c', 52),
                category("free", '.', 848),
            ],
            trigger_tokens: 500,
            compactions: 0,
            last_compaction: None,
            input_tokens: 10,
            output_tokens: 2,
            files: Vec::new(),
            toolsets: Vec::new(),
            skills: Vec::new(),
        }
    }

    #[test]
    fn the_grid_is_five_rows_of_twenty_cells() {
        let text = render(&view(), Style::Grid, false);
        let grid: Vec<&str> = text.lines().skip(1).take(5).collect();
        assert!(
            grid.iter().all(|row| row.trim().chars().count() == 20),
            "{text}"
        );
        assert_eq!(grid[0].trim(), "ssssssssssccccc.....");
        assert!(
            text.contains("Compaction trigger: 500 tokens (348 headroom)"),
            "{text}"
        );
    }

    #[test]
    fn messaging_gets_a_gauge_instead_of_the_grid() {
        let text = render(&view(), Style::Text, false);
        assert_eq!(text.lines().nth(1), Some("███░░░░░░░░░░░░░░░░░"));
    }
}
