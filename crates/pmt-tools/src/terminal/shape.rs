//! Turning raw command output into what the model sees (spec:
//! execution-environments, Foreground result shaping).
//!
//! Order: `transform_terminal_output` hooks, ANSI stripping, secret
//! redaction, then head/tail truncation at `tool_output.max_chars` keeping
//! 40% head and 60% tail around one marker.

use pmt_core::secret::Redactor;

/// A `transform_terminal_output` hook: receives and returns the output.
pub type OutputHook = std::sync::Arc<dyn Fn(String) -> String + Send + Sync>;

/// Removes ANSI escape sequences (CSI, OSC, two-byte escapes) and bare
/// carriage returns that only redraw a line.
#[must_use]
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            '\u{9b}' => skip_csi(&mut chars),
            '\r' if chars.peek() == Some(&'\n') => {}
            _ => out.push(c),
        }
    }
    out
}

fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        // OSC (and DCS/PM/APC): up to BEL or ST (`ESC \`).
        Some(']' | 'P' | '^' | '_') => {
            while let Some(c) = chars.next() {
                if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                    break;
                }
            }
        }
        // Charset selection takes one more character.
        Some('(' | ')' | '*' | '+') => {
            chars.next();
        }
        _ => {}
    }
}

fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    // Parameter and intermediate bytes, then one final byte in @..~.
    for c in chars.by_ref() {
        if ('@'..='~').contains(&c) {
            break;
        }
    }
}

/// Runs the hooks, strips ANSI and redacts secrets.
#[must_use]
pub fn clean(raw: &str, hooks: &[OutputHook], redactor: &Redactor) -> String {
    let hooked = hooks.iter().fold(raw.to_owned(), |text, hook| hook(text));
    redactor.redact_all(&strip_ansi(&hooked))
}

/// The truncation marker between head and tail.
#[must_use]
pub fn marker(omitted: usize, total: usize) -> String {
    format!("\n... [OUTPUT TRUNCATED - {omitted} chars omitted out of {total} total] ...\n")
}

/// Head and tail sizes for `max_chars`: 40% and 60%.
#[must_use]
pub fn split(max_chars: usize) -> (usize, usize) {
    let head = max_chars * 2 / 5;
    (head, max_chars - head)
}

/// `text` cut to `max_chars` characters of content (40% head, 60% tail)
/// around one marker, or unchanged when it fits. Returns the text and
/// whether it was truncated.
#[must_use]
pub fn truncate(text: &str, max_chars: usize) -> (String, bool) {
    let total = text.chars().count();
    if total <= max_chars {
        return (text.to_owned(), false);
    }
    let (head, tail) = split(max_chars);
    let head_end = text.char_indices().nth(head).map_or(text.len(), |(i, _)| i);
    let tail_start = text
        .char_indices()
        .nth(total - tail)
        .map_or(text.len(), |(i, _)| i);
    let mut out = String::with_capacity(head_end + (text.len() - tail_start) + 80);
    out.push_str(&text[..head_end]);
    out.push_str(&marker(total - head - tail, total));
    out.push_str(&text[tail_start..]);
    (out, true)
}

/// What an exit status usually means, when that is worth saying.
#[must_use]
pub fn exit_code_meaning(command: &str, code: i32) -> Option<String> {
    let program = command
        .split_whitespace()
        .find(|w| !w.contains('='))
        .map(|w| w.rsplit('/').next().unwrap_or(w));
    if code == 0 {
        return None;
    }
    let meaning = match (program, code) {
        (Some("grep" | "rg" | "egrep" | "fgrep" | "ag"), 1) => "no lines matched (not an error)",
        (Some("diff" | "cmp"), 1) => "the inputs differ (not an error)",
        (Some("test" | "["), 1) => "the condition is false (not an error)",
        (_, 124) => "timed out",
        (_, 126) => {
            "the command was found but could not be executed (permission denied or not executable)"
        }
        (_, 127) => "command not found",
        (_, 130) => "interrupted (SIGINT)",
        (_, 137) => "killed (SIGKILL), often by the out-of-memory killer or a timeout",
        (_, 139) => "segmentation fault (SIGSEGV)",
        (_, 143) => "terminated (SIGTERM)",
        _ => return None,
    };
    Some(meaning.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{exit_code_meaning, strip_ansi, truncate};

    #[test]
    fn ansi_sequences_are_removed() {
        let text = "\u{1b}[1;31merror\u{1b}[0m: \u{1b}]0;title\u{7}done\r\n";
        assert_eq!(strip_ansi(text), "error: done\n");
    }

    #[test]
    fn oversized_output_keeps_40_percent_head_and_60_percent_tail() {
        let text: String = (0..120_000)
            .map(|i| if i < 60_000 { 'h' } else { 't' })
            .collect();
        let (out, truncated) = truncate(&text, 50_000);
        assert!(truncated);
        let marker = "\n... [OUTPUT TRUNCATED - 70000 chars omitted out of 120000 total] ...\n";
        assert_eq!(out.matches("OUTPUT TRUNCATED").count(), 1);
        let (head, tail) = out.split_once(marker).unwrap();
        assert_eq!(head, &text[..20_000]);
        assert_eq!(tail, &text[90_000..]);
        assert_eq!(truncate("short", 10), ("short".to_owned(), false));
    }

    #[test]
    fn exit_meanings() {
        assert!(
            exit_code_meaning("grep foo x", 1)
                .unwrap()
                .contains("no lines")
        );
        assert_eq!(exit_code_meaning("ls", 127).unwrap(), "command not found");
        assert_eq!(exit_code_meaning("ls", 2), None);
    }
}
