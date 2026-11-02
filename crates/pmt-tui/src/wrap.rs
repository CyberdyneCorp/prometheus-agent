//! Word wrapping by display width (wide characters count two columns).

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

/// `text` wrapped to `width` columns: each `\n` starts a line, words move
/// to the next line when they do not fit, and a word longer than a line is
/// broken. An empty text is one empty line.
#[must_use]
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_inclusive(' ') {
            if line.width() + word.trim_end().width() > width && !line.is_empty() {
                lines.push(line.trim_end().to_owned());
                line.clear();
            }
            for c in word.chars() {
                push_char(&mut lines, &mut line, c, width);
            }
        }
        lines.push(line.trim_end().to_owned());
    }
    lines
}

/// Adds `c` to `line`, first ending the line when `c` does not fit; a
/// space that does not fit ends the line and is not shown.
fn push_char(lines: &mut Vec<String>, line: &mut String, c: char, width: usize) {
    if line.width() + c.width().unwrap_or(0) <= width {
        line.push(c);
        return;
    }
    lines.push(std::mem::take(line));
    if c != ' ' {
        line.push(c);
    }
}

/// Styled runs wrapped like [`wrap`]: each line is the runs' characters
/// that fall on it, with their styles.
#[must_use]
pub fn wrap_spans(runs: &[(String, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    let text: String = runs.iter().map(|(t, _)| t.as_str()).collect();
    let mut styles = runs
        .iter()
        .flat_map(|(t, style)| t.chars().filter(|c| *c != '\n').map(move |_| *style));
    let mut lines = Vec::new();
    for (line, paragraph) in wrap_with_breaks(&text, width) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut shown = line.chars();
        for c in paragraph.chars() {
            let style = styles.next().unwrap_or_default();
            // Spaces wrap drops at a break consume a style but show nothing.
            if shown.clone().next() != Some(c) {
                continue;
            }
            shown.next();
            match spans.last_mut() {
                Some(span) if span.style == style => span.content.to_mut().push(c),
                _ => spans.push(Span::styled(c.to_string(), style)),
            }
        }
        lines.push(spans);
    }
    lines
}

/// [`wrap`]'s lines, each with the source characters it covers (its own
/// and the spaces dropped after it).
fn wrap_with_breaks(text: &str, width: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let lines = wrap(paragraph, width);
        let mut rest = paragraph;
        let count = lines.len();
        for (n, line) in lines.into_iter().enumerate() {
            let take = if n + 1 == count {
                rest.len()
            } else {
                covered(rest, &line)
            };
            out.push((line, rest[..take].to_owned()));
            rest = &rest[take..];
        }
    }
    out
}

/// How many bytes of `rest` the wrapped `line` covers: its characters,
/// then the spaces the break dropped.
fn covered(rest: &str, line: &str) -> usize {
    let mut end = 0;
    let mut want = line.chars().peekable();
    for (at, c) in rest.char_indices() {
        match want.peek() {
            Some(w) if *w == c => {
                want.next();
                end = at + c.len_utf8();
            }
            None if c == ' ' => end = at + 1,
            _ => break,
        }
    }
    end
}

/// `text` cut to `width` columns, ending in `…` when it was cut.
#[must_use]
pub fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    for c in text.chars() {
        if out.width() + c.width().unwrap_or(0) + 1 > width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use super::{truncate, wrap, wrap_spans};

    #[test]
    fn styled_runs_wrap_like_text_and_keep_their_styles() {
        let code = Style::new().fg(Color::Green);
        let runs = [
            ("run ".to_owned(), Style::new()),
            ("cargo build".to_owned(), code),
            (" now\nnext".to_owned(), Style::new()),
        ];
        let lines = wrap_spans(&runs, 10);
        let texts: Vec<Vec<(String, bool)>> = lines
            .iter()
            .map(|l| {
                l.iter()
                    .map(|s| (s.content.to_string(), s.style == code))
                    .collect()
            })
            .collect();
        let expected: Vec<Vec<(String, bool)>> = vec![
            vec![("run ".into(), false), ("cargo".into(), true)],
            vec![("build".into(), true), (" now".into(), false)],
            vec![("next".into(), false)],
        ];
        assert_eq!(texts, expected);
    }

    #[test]
    fn words_move_to_the_next_line_and_long_ones_break() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(wrap("", 10), [""]);
        assert_eq!(
            wrap("界界界", 4),
            ["界界", "界"],
            "wide characters are two columns"
        );
    }

    #[test]
    fn truncation_marks_the_cut() {
        assert_eq!(truncate("profile default", 8), "profile…");
        assert_eq!(truncate("short", 8), "short");
    }
}
