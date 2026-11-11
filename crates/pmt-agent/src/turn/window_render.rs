//! How `/context` shows a [`ContextView`]: a 5×20 cell grid on the CLI,
//! the TUI and the desktop, a text gauge on messaging platforms (spec:
//! context-compression, Context window view).

use std::fmt::{Display, Write as _};

use pmt_core::text::Catalog;

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

/// The catalog key of a category or file-state label (`MCP tools` ->
/// `slash.context.label.mcp_tools`).
fn label(text: &dyn Catalog, name: &str) -> String {
    let key = format!(
        "slash.context.label.{}",
        name.to_lowercase().replace(' ', "_")
    );
    text.text(&key, &[])
}

/// The view as text, worded with `text`.
#[must_use]
pub fn render(text: &dyn Catalog, view: &ContextView, style: Style, all: bool) -> String {
    let used = view.used();
    let mut out = String::new();
    let mut line = |key: &str, args: &[(&str, &dyn Display)]| {
        let _ = writeln!(out, "{}", text.text(key, args));
    };
    let percent = format!("{:.0}", share(used, view.window));
    line(
        "slash.context.window",
        &[
            ("used", &used),
            ("window", &view.window),
            ("percent", &percent),
        ],
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
        let row = text.text(
            "slash.context.category",
            &[
                ("name", &label(text, c.name)),
                ("tokens", &c.tokens),
                ("percent", &format!("{:.1}", share(c.tokens, view.window))),
            ],
        );
        let _ = writeln!(out, "{glyph}{row}");
    }
    let headroom = view.trigger_tokens.saturating_sub(used);
    let last = view.last_compaction.map_or_else(
        || text.text("slash.context.never", &[]),
        |at| at.strftime("%Y-%m-%d %H:%M").to_string(),
    );
    let mut line = |key: &str, args: &[(&str, &dyn Display)]| {
        let _ = writeln!(out, "{}", text.text(key, args));
    };
    line(
        "slash.context.trigger",
        &[("tokens", &view.trigger_tokens), ("headroom", &headroom)],
    );
    line(
        "slash.context.compactions",
        &[("count", &view.compactions), ("last", &last)],
    );
    line(
        "slash.context.throughput",
        &[
            ("input", &view.input_tokens),
            ("output", &view.output_tokens),
        ],
    );
    files(text, &mut out, view);
    if all {
        details(text, &mut out, view);
    }
    out.trim_end().to_owned()
}

fn files(text: &dyn Catalog, out: &mut String, view: &ContextView) {
    if view.files.is_empty() {
        let _ = writeln!(out, "{}", text.text("slash.context.no_files", &[]));
        return;
    }
    let _ = writeln!(out, "{}", text.text("slash.context.files", &[]));
    for file in &view.files {
        let row = text.text(
            "slash.context.file",
            &[
                ("name", &file.name),
                ("tokens", &file.tokens),
                ("state", &label(text, file.state.as_str())),
            ],
        );
        let _ = writeln!(out, "  {row}");
    }
}

fn details(text: &dyn Catalog, out: &mut String, view: &ContextView) {
    let _ = writeln!(out, "{}", text.text("slash.context.toolsets", &[]));
    for set in &view.toolsets {
        let row = text.text(
            "slash.context.toolset",
            &[
                ("name", &set.toolset),
                ("tools", &set.tools),
                ("tokens", &set.tokens),
            ],
        );
        let _ = writeln!(out, "  {row}");
    }
    if !view.skills.is_empty() {
        let _ = writeln!(out, "{}", text.text("slash.context.skills", &[]));
        for skill in &view.skills {
            let row = text.text(
                "slash.context.skill",
                &[
                    ("name", &skill.name),
                    ("index", &skill.index_tokens),
                    ("full", &skill.full_tokens),
                ],
            );
            let _ = writeln!(out, "  {row}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::window::Category;

    fn en() -> std::sync::Arc<pmt_i18n::Strings> {
        pmt_i18n::Strings::english(pmt_i18n::Surface::Core)
    }

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
        let text = render(&*en(), &view(), Style::Grid, false);
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
        let text = render(&*en(), &view(), Style::Text, false);
        assert_eq!(text.lines().nth(1), Some("███░░░░░░░░░░░░░░░░░"));
    }
}
