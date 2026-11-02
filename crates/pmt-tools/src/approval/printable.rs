//! Model-controlled approval text made safe to print on a terminal.
//!
//! An approval prompt shows the command, path or fragment the model chose.
//! Printed raw, a carriage return, an ANSI sequence (`ESC[2K`, `ESC[1A`)
//! or a bidi override could hide or rewrite part of it before the user
//! decides, so every surface that prints one to a terminal goes through
//! [`printable`] first.

use std::fmt::Write as _;

use super::detectors::is_invisible;

/// `text` with every control character except newline, and every
/// invisible formatting character, replaced by a visible escape: `^[` for
/// ESC, `\r`, `\t`, and `\u{..}` for the rest.
#[must_use]
pub fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push('\n'),
            '\u{1b}' => out.push_str("^["),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || is_invisible(c) => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::printable;

    #[test]
    fn controls_and_invisible_characters_become_visible() {
        assert_eq!(printable("rm -rf ~\u{1b}[2K\rls"), "rm -rf ~^[[2K\\rls");
        assert_eq!(
            printable("a\u{202e}b\u{7}c\u{9b}"),
            "a\\u{202e}b\\u{7}c\\u{9b}"
        );
        assert_eq!(printable("line one\nline two é"), "line one\nline two é");
    }
}
