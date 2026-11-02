//! Word wrapping by display width (wide characters count two columns).

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
    use super::{truncate, wrap};

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
