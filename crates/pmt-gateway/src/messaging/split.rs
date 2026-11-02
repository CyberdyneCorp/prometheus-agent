//! Fence-aware message splitting (spec: platform-adapters, Fence-aware
//! message splitting): a reply longer than the platform's limit, measured
//! in the descriptor's unit, is cut preferring paragraph, then line, then
//! word boundaries, never inside an inline code span or a character; an
//! open code fence is closed at a part's end and reopened with its
//! language tag in the next part; every part of a multi-part reply ends
//! with ` (i/N)`, counted within the limit. Any limit of at least 16
//! units terminates.

use super::descriptor::LengthUnit;

/// Splits `text` into parts of at most `limit` units.
#[must_use]
pub fn split(text: &str, limit: usize, unit: LengthUnit) -> Vec<String> {
    let limit = limit.max(1);
    if unit.len(text) <= limit {
        return vec![text.to_owned()];
    }
    let mut digits = 1;
    loop {
        // " (i/N)" with i and N of `digits` digits each.
        let reserve = 4 + 2 * digits;
        if reserve >= limit {
            // Indicators cannot fit: plain parts still terminate.
            return Splitter { limit, unit }.parts(text);
        }
        let parts = Splitter {
            limit: limit - reserve,
            unit,
        }
        .parts(text);
        if parts.len().to_string().len() <= digits {
            let total = parts.len();
            return parts
                .into_iter()
                .enumerate()
                .map(|(i, part)| format!("{part} ({}/{total})", i + 1))
                .collect();
        }
        digits += 1;
    }
}

struct Splitter {
    limit: usize,
    unit: LengthUnit,
}

/// The closing fence appended to a part that ends inside a code block.
const CLOSE: &str = "\n```";

impl Splitter {
    fn parts(&self, text: &str) -> Vec<String> {
        let mut parts = Vec::new();
        let mut rest = text;
        let mut open: Option<String> = None;
        while !rest.is_empty() {
            let (part, used, still_open) = self.next_part(rest, open.as_deref());
            parts.push(part);
            open = still_open;
            rest = skip_separator(&rest[used..]);
        }
        parts
    }

    /// One part of `rest`, the bytes of `rest` it used and the fence open
    /// at its end.
    fn next_part(&self, rest: &str, open: Option<&str>) -> (String, usize, Option<String>) {
        let prefix = open.map(|fence| format!("{fence}\n")).unwrap_or_default();
        let room = self.limit.saturating_sub(self.unit.len(&prefix));
        let close_len = self.unit.len(CLOSE);
        if room <= close_len {
            // Too small to carry fences: a plain cut always progresses.
            let cut = cut_at(rest, self.limit, self.unit);
            return (rest[..cut].to_owned(), cut, None);
        }
        let mut cut = best_cut(rest, room, self.unit);
        let mut fence = fence_after(open, &rest[..cut]);
        if fence.is_some() && cut < rest.len() {
            cut = best_cut(rest, room - close_len, self.unit);
            fence = fence_after(open, &rest[..cut]);
        }
        let body = &rest[..cut];
        let part = match (&fence, cut < rest.len()) {
            (Some(_), true) => format!("{prefix}{}{CLOSE}", body.trim_end_matches('\n')),
            (_, true) => format!("{prefix}{}", body.trim_end()),
            _ => format!("{prefix}{body}"),
        };
        let carried = if cut < rest.len() { fence } else { None };
        (part, cut, carried)
    }
}

/// Drops the blank lines a cut left at the start of the next part (a
/// cut is made after its separator, so indentation stays).
fn skip_separator(rest: &str) -> &str {
    rest.trim_start_matches('\n')
}

/// The largest char boundary whose prefix fits `room` (at least one
/// character, so splitting always progresses).
fn cut_at(text: &str, room: usize, unit: LengthUnit) -> usize {
    let mut used = 0;
    for (at, c) in text.char_indices() {
        used += unit.of(c);
        if used > room {
            return if at == 0 { c.len_utf8() } else { at };
        }
    }
    text.len()
}

/// The cut within `room` at the best boundary: a paragraph, a line, a
/// word, outside inline code; else as much as fits.
fn best_cut(text: &str, room: usize, unit: LengthUnit) -> usize {
    let hard = cut_at(text, room, unit);
    if hard == text.len() {
        return hard;
    }
    let window = &text[..hard];
    let floor = hard / 3;
    for separator in ["\n\n", "\n", " "] {
        let found = window
            .match_indices(separator)
            .map(|(at, s)| at + s.len())
            .filter(|at| *at > floor && !in_inline_code(text, *at))
            .last();
        if let Some(at) = found {
            return at;
        }
    }
    match inline_code_start(text, hard) {
        Some(start) if start > 0 => start,
        _ => hard,
    }
}

/// Whether byte `at` of `text` falls inside an inline code span of its
/// line.
fn in_inline_code(text: &str, at: usize) -> bool {
    inline_code_start(text, at).is_some()
}

/// The start of the inline code span `at` falls in.
fn inline_code_start(text: &str, at: usize) -> Option<usize> {
    let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    let line = &text[line_start..at];
    if line.trim_start().starts_with("```") {
        return None;
    }
    let ticks: Vec<usize> = line.match_indices('`').map(|(i, _)| i).collect();
    (ticks.len() % 2 == 1).then(|| line_start + ticks[ticks.len() - 1])
}

/// The fence open after `body`, given the one open before it.
fn fence_after(open: Option<&str>, body: &str) -> Option<String> {
    let mut fence = open.map(str::to_owned);
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            fence = match fence {
                Some(_) => None,
                None => Some(trimmed.to_owned()),
            };
        }
    }
    fence
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn without_indicator(part: &str) -> &str {
        part.rsplit_once(" (").map_or(part, |(body, _)| body)
    }

    #[test]
    fn a_code_fence_is_carried_across_parts() {
        let code: String = (0..250).fold(String::new(), |mut code, i| {
            let _ = writeln!(code, "print({i:05})        ");
            code
        });
        assert!(code.chars().count() >= 5000);
        let reply = format!("Here is the script:\n\n```python\n{code}```\nDone.");
        let parts = split(&reply, 2000, LengthUnit::Chars);
        assert!(parts.len() >= 3, "{}", parts.len());
        for (i, part) in parts.iter().enumerate() {
            assert!(part.chars().count() <= 2000, "part {i} too long");
            assert!(part.ends_with(&format!(" ({}/{})", i + 1, parts.len())));
            if i + 1 < parts.len() {
                assert!(without_indicator(part).ends_with("```"), "part {i}: {part}");
            }
            if i > 0 {
                assert!(part.starts_with("```python\n"), "part {i}");
            }
        }
    }

    #[test]
    fn utf16_limits_count_astral_characters_twice() {
        let reply: String = (0..4000)
            .map(|i| if i % 20 == 0 { '\u{1f600}' } else { 'a' })
            .collect();
        assert_eq!(reply.chars().count(), 4000);
        let parts = split(&reply, 4096, LengthUnit::Utf16);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.encode_utf16().count() <= 4096));
        let joined: String = parts.iter().map(|p| without_indicator(p)).collect();
        assert_eq!(joined.chars().filter(|c| *c == '\u{1f600}').count(), 200);
    }

    #[test]
    fn a_pathological_limit_terminates() {
        let word = "x".repeat(1000);
        let parts = split(&word, 16, LengthUnit::Chars);
        assert!(parts.iter().all(|p| p.chars().count() <= 16));
        let total: usize = parts.iter().map(|p| without_indicator(p).len()).sum();
        assert_eq!(total, 1000);
    }

    #[test]
    fn boundaries_prefer_paragraphs_then_lines_then_words_outside_code() {
        let parts = split("aaaa aaaa\n\nbbbb bbbb cccc", 20, LengthUnit::Chars);
        assert_eq!(without_indicator(&parts[0]), "aaaa aaaa");
        let parts = split("one `inline code span here` two", 24, LengthUnit::Chars);
        assert!(
            parts.iter().any(|p| p.contains("`inline code span")),
            "{parts:?}"
        );
        assert_eq!(split("short", 100, LengthUnit::Chars), ["short"]);
    }
}
