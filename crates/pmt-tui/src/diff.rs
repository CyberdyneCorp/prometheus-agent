//! A proposed file edit on an approval card (spec: tui, Inline diff
//! review for file edits): a unified diff whose hunks are numbered (the
//! numbers `accept selected hunks` names), its headers, removed and added
//! lines each in their own colour.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme::Theme;
use crate::wrap::truncate;

/// One hunk of the edit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffHunk {
    /// `@@ -1,4 +1,4 @@`.
    pub header: String,
    /// Its lines, each starting with ` `, `-` or `+`.
    pub lines: Vec<String>,
}

/// The style of one diff line.
fn style(line: &str, theme: &Theme) -> Style {
    match line.chars().next() {
        Some('+') => theme.added,
        Some('-') => theme.removed,
        _ => theme.card,
    }
}

/// The hunks as card lines, `[n]` before each header, cut to `width`.
#[must_use]
pub fn lines(hunks: &[DiffHunk], width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let width = usize::from(width).max(1);
    let mut lines = Vec::new();
    for (n, hunk) in hunks.iter().enumerate() {
        let header = format!("  [{}] {}", n + 1, hunk.header);
        lines.push(Line::from(Span::styled(
            truncate(&header, width),
            theme.hunk,
        )));
        for line in &hunk.lines {
            let shown = truncate(&format!("    {line}"), width);
            lines.push(Line::from(Span::styled(shown, style(line, theme))));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::TuiTheme;

    use super::{DiffHunk, lines};
    use crate::theme::Theme;

    #[test]
    fn hunks_are_numbered_and_lines_coloured_by_kind() {
        let theme = Theme::new(TuiTheme::Dark, false);
        let hunks = vec![DiffHunk {
            header: "@@ -1,2 +1,2 @@".into(),
            lines: vec![" keep".into(), "-old".into(), "+new".into()],
        }];
        let shown = lines(&hunks, 40, &theme);
        assert_eq!(shown[0].spans[0].content, "  [1] @@ -1,2 +1,2 @@");
        assert_eq!(shown[0].spans[0].style, theme.hunk);
        assert_eq!(shown[2].spans[0].style, theme.removed);
        assert_eq!(shown[3].spans[0].style, theme.added);
    }
}
