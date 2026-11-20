//! A proposed file edit on an approval card (spec: tui, Inline diff
//! review for file edits): a unified diff whose hunks are numbered (the
//! numbers `accept selected hunks` names), its headers, removed and added
//! lines each in their own colour, and the code on them highlighted by
//! its language's syntax.

use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar as _;

use crate::theme::Theme;
use crate::wrap::truncate;

/// What a run of code is, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syntax {
    /// A keyword.
    Keyword,
    /// A string or character literal.
    String,
    /// A comment.
    Comment,
    /// A number, boolean or null-like literal.
    Constant,
    /// A type name.
    Type,
}

/// One hunk of the edit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffHunk {
    /// `@@ -1,4 +1,4 @@`.
    pub header: String,
    /// Its lines, each starting with ` `, `-` or `+`.
    pub lines: Vec<String>,
    /// The syntax runs of each line's code (byte ranges after its sign),
    /// `syntax[n]` for `lines[n]`; shorter, or empty, when the language
    /// is not known.
    pub syntax: Vec<Vec<(Range<usize>, Syntax)>>,
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
        for (n, line) in hunk.lines.iter().enumerate() {
            let runs = hunk.syntax.get(n).map_or(&[][..], Vec::as_slice);
            lines.push(Line::from(code_line(line, runs, width, theme)));
        }
    }
    lines
}

/// One diff line as spans, cut to `width`: the indent and sign in the
/// line's colour, its code in its syntax colours.
fn code_line(
    line: &str,
    runs: &[(Range<usize>, Syntax)],
    width: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let base = style(line, theme);
    let sign = line.chars().next().map_or(0, char::len_utf8);
    let code = &line[sign..];
    let mut pieces = vec![(format!("    {}", &line[..sign]), base)];
    let mut at = 0;
    for (range, class) in runs {
        // Runs come from the line's own text; skip any that do not fit it.
        let (Some(before), Some(run)) = (code.get(at..range.start), code.get(range.clone())) else {
            continue;
        };
        pieces.push((before.to_owned(), base));
        pieces.push((run.to_owned(), theme.syntax(*class)));
        at = range.end;
    }
    pieces.push((code.get(at..).unwrap_or_default().to_owned(), base));
    cut(pieces, width)
}

/// `pieces` cut to `width` columns, ending in `…` when cut (as
/// [`truncate`] cuts plain text).
fn cut(pieces: Vec<(String, Style)>, width: usize) -> Vec<Span<'static>> {
    let whole: String = pieces.iter().map(|(text, _)| text.as_str()).collect();
    let fits = truncate(&whole, width);
    if fits == whole {
        return pieces
            .into_iter()
            .filter(|(text, _)| !text.is_empty())
            .map(|(text, style)| Span::styled(text, style))
            .collect();
    }
    let mut room = fits.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() - 1;
    let mut spans = Vec::new();
    let mut last = Style::new();
    for (text, style) in pieces {
        let kept: String = text
            .chars()
            .take_while(|c| {
                let w = c.width().unwrap_or(0);
                let fit = w <= room;
                if fit {
                    room -= w;
                }
                fit
            })
            .collect();
        last = style;
        let short = kept.len() < text.len();
        if !kept.is_empty() {
            spans.push(Span::styled(kept, style));
        }
        if short {
            break;
        }
    }
    spans.push(Span::styled("…", last));
    spans
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::TuiTheme;

    use super::{DiffHunk, Syntax, lines};
    use crate::theme::Theme;

    #[test]
    fn hunks_are_numbered_and_lines_coloured_by_kind() {
        let theme = Theme::new(TuiTheme::Dark, false);
        let hunks = vec![DiffHunk {
            header: "@@ -1,2 +1,2 @@".into(),
            lines: vec![" keep".into(), "-old".into(), "+new".into()],
            syntax: Vec::new(),
        }];
        let shown = lines(&hunks, 40, &theme);
        assert_eq!(shown[0].spans[0].content, "  [1] @@ -1,2 +1,2 @@");
        assert_eq!(shown[0].spans[0].style, theme.hunk);
        assert_eq!(shown[2].spans[0].style, theme.removed);
        assert_eq!(shown[3].spans[0].style, theme.added);
    }

    #[test]
    fn code_is_coloured_by_its_syntax_and_the_sign_by_the_line() {
        let theme = Theme::new(TuiTheme::Dark, false);
        let hunks = vec![DiffHunk {
            header: "@@ -1 +1 @@".into(),
            lines: vec!["+let s = \"hi\";".into()],
            syntax: vec![vec![(0..3, Syntax::Keyword), (8..12, Syntax::String)]],
        }];
        let shown = lines(&hunks, 40, &theme);
        let spans: Vec<_> = shown[1]
            .spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style))
            .collect();
        assert_eq!(
            spans,
            [
                ("    +", theme.added),
                ("let", theme.syntax(Syntax::Keyword)),
                (" s = ", theme.added),
                ("\"hi\"", theme.syntax(Syntax::String)),
                (";", theme.added),
            ]
        );
        // Cut to the width like plain text, the cut run keeping its colour.
        let cut = lines(&hunks, 14, &theme);
        let text: String = cut[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "    +let s = …");
        assert_eq!(cut[1].spans[1].style, theme.syntax(Syntax::Keyword));
    }
}
