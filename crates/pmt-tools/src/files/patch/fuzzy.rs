//! Locating `old_string` in a file (spec: execution-environments, patch
//! tool contract).
//!
//! Strategies run in a fixed order and the first one that finds anything
//! decides: exact, line-trimmed, trailing-whitespace-normalized,
//! indentation-flexible, unicode-normalized, escape-normalized,
//! whitespace-collapsed, block-anchor and context-similarity. Every
//! strategy but exact and escape-normalized compares whole lines, so a
//! match is a run of complete lines.

use similar::TextDiff;

/// A strategy's name (reported as `match_strategy` when not exact).
pub type StrategyName = &'static str;

/// Byte range of one match in the file.
pub type Span = (usize, usize);

/// Similarity a window needs for context-similarity matching.
const CONTEXT_SIMILARITY: f64 = 0.8;

/// Similarity the middle of a block-anchor match needs.
const ANCHOR_MIDDLE_SIMILARITY: f64 = 0.5;

/// The matches of the first strategy that finds any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The deciding strategy.
    pub strategy: StrategyName,
    /// Non-overlapping matches, in file order.
    pub spans: Vec<Span>,
}

/// Runs the strategies in order.
#[must_use]
pub fn find(content: &str, old: &str) -> Option<Found> {
    let file = Lines::new(content);
    let pattern = pattern_lines(old);
    let lines = |normalize: fn(&str) -> String| file.spans(&by_lines(&file, &pattern, normalize));
    let strategies: [(StrategyName, &dyn Fn() -> Vec<Span>); 9] = [
        ("exact", &|| exact(content, old)),
        ("line_trimmed", &|| lines(|l| l.trim().to_owned())),
        ("trailing_whitespace", &|| {
            lines(|l| l.trim_end().to_owned())
        }),
        ("indentation_flexible", &|| {
            lines(|l| l.trim_start().to_owned())
        }),
        ("unicode_normalized", &|| {
            lines(|l| unicode_normalize(l).trim().to_owned())
        }),
        ("escape_normalized", &|| escape_normalized(content, old)),
        ("whitespace_collapsed", &|| lines(collapse_whitespace)),
        ("block_anchor", &|| {
            file.spans(&block_anchor(&file, &pattern))
        }),
        ("context_similarity", &|| {
            file.spans(&context_similarity(&file, &pattern))
        }),
    ];
    strategies.iter().find_map(|(strategy, run)| {
        let spans = run();
        (!spans.is_empty()).then_some(Found { strategy, spans })
    })
}

/// The window of lines most similar to `old`, for the "closest candidate"
/// shown when nothing matched: `(first line, last line, similarity)`.
#[must_use]
pub fn closest(content: &str, old: &str) -> Option<(usize, usize, f64)> {
    let file = Lines::new(content);
    let pattern = pattern_lines(old);
    let width = pattern.len().max(1);
    if file.lines.is_empty() {
        return None;
    }
    let wanted = pattern.join("\n");
    (0..=file.lines.len().saturating_sub(width))
        .map(|start| {
            let end = (start + width).min(file.lines.len());
            let window = file.lines[start..end].join("\n");
            (start + 1, end, ratio(&window, &wanted))
        })
        .max_by(|a, b| a.2.total_cmp(&b.2))
}

fn exact(content: &str, old: &str) -> Vec<Span> {
    if old.is_empty() {
        return Vec::new();
    }
    content
        .match_indices(old)
        .map(|(start, text)| (start, start + text.len()))
        .collect()
}

/// `old` written with literal escapes (`\n`, `\t`, `\"`): unescape and
/// search again.
fn escape_normalized(content: &str, old: &str) -> Vec<Span> {
    if !old.contains('\\') {
        return Vec::new();
    }
    let unescaped = old
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\"", "\"")
        .replace("\\'", "'")
        .replace("\\\\", "\\");
    if unescaped == old {
        return Vec::new();
    }
    exact(content, &unescaped)
}

fn unicode_normalize(line: &str) -> String {
    line.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2032}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2007}' | '\u{202F}' => ' ',
            other => other,
        })
        .collect()
}

fn collapse_whitespace(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The lines of `old`; a final newline does not add an empty line.
fn pattern_lines(old: &str) -> Vec<&str> {
    let body = old.strip_suffix('\n').unwrap_or(old);
    body.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

/// A file split into lines with each line's byte range.
struct Lines<'a> {
    lines: Vec<&'a str>,
    ranges: Vec<Span>,
}

impl<'a> Lines<'a> {
    fn new(content: &'a str) -> Self {
        let mut lines = Vec::new();
        let mut ranges = Vec::new();
        let mut start = 0;
        for piece in content.split_inclusive('\n') {
            let end = start + piece.len();
            let text = piece.strip_suffix('\n').unwrap_or(piece);
            let text = text.strip_suffix('\r').unwrap_or(text);
            lines.push(text);
            ranges.push((start, start + text.len()));
            start = end;
        }
        Self { lines, ranges }
    }

    /// Byte spans of line windows `(first, count)` (0-based first line).
    fn spans(&self, windows: &[(usize, usize)]) -> Vec<Span> {
        windows
            .iter()
            .map(|&(first, count)| (self.ranges[first].0, self.ranges[first + count - 1].1))
            .collect()
    }
}

/// Non-overlapping windows whose lines equal the pattern's after
/// `normalize`.
fn by_lines(
    file: &Lines<'_>,
    pattern: &[&str],
    normalize: fn(&str) -> String,
) -> Vec<(usize, usize)> {
    let want: Vec<String> = pattern.iter().map(|l| normalize(l)).collect();
    if want.iter().all(String::is_empty) {
        return Vec::new();
    }
    let have: Vec<String> = file.lines.iter().map(|l| normalize(l)).collect();
    let mut windows = Vec::new();
    let mut start = 0;
    while start + want.len() <= have.len() {
        if have[start..start + want.len()] == want[..] {
            windows.push((start, want.len()));
            start += want.len();
        } else {
            start += 1;
        }
    }
    windows
}

/// Windows of the pattern's height whose first and last lines equal the
/// pattern's (trimmed) and whose middle is similar enough. Needs three
/// lines or more.
fn block_anchor(file: &Lines<'_>, pattern: &[&str]) -> Vec<(usize, usize)> {
    let height = pattern.len();
    if height < 3 || file.lines.len() < height {
        return Vec::new();
    }
    let (first, last) = (pattern[0].trim(), pattern[height - 1].trim());
    if first.is_empty() || last.is_empty() {
        return Vec::new();
    }
    let middle = pattern[1..height - 1].join("\n");
    (0..=file.lines.len() - height)
        .filter(|&start| {
            file.lines[start].trim() == first
                && file.lines[start + height - 1].trim() == last
                && ratio(
                    &file.lines[start + 1..start + height - 1].join("\n"),
                    &middle,
                ) >= ANCHOR_MIDDLE_SIMILARITY
        })
        .map(|start| (start, height))
        .collect()
}

/// The best window(s) of the pattern's height at
/// [`CONTEXT_SIMILARITY`] or more.
fn context_similarity(file: &Lines<'_>, pattern: &[&str]) -> Vec<(usize, usize)> {
    let height = pattern.len();
    if file.lines.len() < height || pattern.iter().all(|l| l.trim().is_empty()) {
        return Vec::new();
    }
    let wanted = pattern.join("\n");
    let scored: Vec<(usize, f64)> = (0..=file.lines.len() - height)
        .map(|start| {
            (
                start,
                ratio(&file.lines[start..start + height].join("\n"), &wanted),
            )
        })
        .filter(|(_, score)| *score >= CONTEXT_SIMILARITY)
        .collect();
    let best = scored.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max);
    scored
        .into_iter()
        .filter(|(_, s)| (*s - best).abs() < f64::EPSILON)
        .map(|(start, _)| (start, height))
        .collect()
}

fn ratio(a: &str, b: &str) -> f64 {
    f64::from(TextDiff::from_chars(a, b).ratio())
}

#[cfg(test)]
mod tests {
    use super::{closest, find};

    fn strategy(content: &str, old: &str) -> Option<(&'static str, usize)> {
        find(content, old).map(|f| (f.strategy, f.spans.len()))
    }

    #[test]
    fn strategies_fire_in_order() {
        let file = "fn a() {\n    let x = \u{201C}hi\u{201D};   \n    call( 1,  2 );\n}\n";
        assert_eq!(strategy(file, "fn a() {"), Some(("exact", 1)));
        assert_eq!(strategy(file, "  fn a() {  "), Some(("line_trimmed", 1)));
        assert_eq!(
            strategy(file, "let x = \"hi\";"),
            Some(("unicode_normalized", 1))
        );
        assert_eq!(
            strategy(file, "call( 1, 2 );"),
            Some(("whitespace_collapsed", 1))
        );
        assert_eq!(
            strategy("a\n\tb\n", "a\\n\\tb"),
            Some(("escape_normalized", 1))
        );
    }

    #[test]
    fn block_anchor_and_context_similarity_find_drifted_blocks() {
        let file = "start()\nmiddle_one(1)\nmiddle_two(2)\nend()\nother()\n";
        let drifted = "start()\nmiddle_one(9)\nmiddle_two(2)\nend()";
        assert_eq!(strategy(file, drifted), Some(("block_anchor", 1)));
        let near = "middle_one(1)\nmiddle_tw0(2)";
        assert_eq!(strategy(file, near), Some(("context_similarity", 1)));
    }

    #[test]
    fn spans_cover_whole_lines_and_closest_finds_the_region() {
        let file = "a\n  b\nc\n";
        let found = find(file, "b").unwrap();
        assert_eq!(&file[found.spans[0].0..found.spans[0].1], "b");
        let found = find(file, "   b").unwrap();
        assert_eq!(&file[found.spans[0].0..found.spans[0].1], "  b");
        assert_eq!(closest(file, "zzz c").map(|c| (c.0, c.1)), Some((3, 3)));
    }
}
