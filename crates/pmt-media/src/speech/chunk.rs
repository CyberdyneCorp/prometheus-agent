//! Sentence-aware splitting of long text under a provider's per-request
//! cap (spec: web-and-media-tools, `text_to_speech` tool and delivery:
//! never truncating silently).
//!
//! Text is cut at the last sentence end that fits, else the last
//! whitespace, else exactly at the cap (a single unbroken run longer than
//! the cap). The chunks together hold every character, in order.

/// Splits `text` into ordered chunks of at most `cap` characters.
#[must_use]
pub fn chunks(text: &str, cap: usize) -> Vec<String> {
    let cap = cap.max(1);
    let mut out = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        if rest.chars().count() <= cap {
            out.push(rest.to_owned());
            break;
        }
        let limit = rest
            .char_indices()
            .nth(cap)
            .map_or(rest.len(), |(at, _)| at);
        let window = &rest[..limit];
        let cut = sentence_end(window)
            .or_else(|| window.rfind(char::is_whitespace).filter(|at| *at > 0))
            .unwrap_or(limit);
        let (chunk, tail) = rest.split_at(cut);
        let chunk = chunk.trim();
        if !chunk.is_empty() {
            out.push(chunk.to_owned());
        }
        rest = tail.trim_start();
    }
    out
}

/// The byte offset just after the last sentence end of `window` (a `.`,
/// `!`, `?` or newline followed by whitespace or the window's end).
fn sentence_end(window: &str) -> Option<usize> {
    let mut best = None;
    let mut chars = window.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let next_blank = chars.peek().is_none_or(|(_, n)| n.is_whitespace());
        if matches!(c, '.' | '!' | '?' | '\n' | '。' | '！' | '？') && next_blank {
            best = Some(at + c.len_utf8());
        }
    }
    best.filter(|at| *at > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_thousand_characters_under_a_4096_cap_make_three_ordered_chunks() {
        let sentence = "The quick brown fox jumps over the lazy dog again. ";
        let text: String = sentence.repeat(10_000 / sentence.len() + 1)[..10_000].to_owned();
        let parts = chunks(&text, 4_096);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.chars().count() <= 4_096));
        assert!(parts[0].ends_with('.'));
        let joined: String = parts.join(" ");
        let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(squash(&joined), squash(&text));
    }

    #[test]
    fn unbroken_runs_are_cut_at_the_cap_and_nothing_is_lost() {
        let text = "x".repeat(25);
        let parts = chunks(&text, 10);
        assert_eq!(parts, ["x".repeat(10), "x".repeat(10), "x".repeat(5)]);
        assert_eq!(chunks("short. text", 100), ["short. text"]);
        assert!(chunks("   ", 10).is_empty());
    }
}
