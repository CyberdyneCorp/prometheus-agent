//! The provider recall budget (spec: memory, External provider recall
//! budget and spill): recall up to `memory.provider_recall_max_chars` goes
//! into the fence whole; beyond it, the text is cut with a marker, or, with
//! `memory.provider_recall_spill`, replaced by a head and tail preview and
//! the path of a file under the profile's `cache/` holding all of it
//! (readable with `read_file`). Built-in recall keeps its own bound.

use std::path::Path;

/// Characters of the head and the tail a spilled recall keeps.
pub const PREVIEW_HEAD: usize = 2000;
/// See [`PREVIEW_HEAD`].
pub const PREVIEW_TAIL: usize = 1000;

fn head(text: &str, chars: usize) -> &str {
    text.char_indices()
        .nth(chars)
        .map_or(text, |(i, _)| &text[..i])
}

fn tail(text: &str, chars: usize) -> &str {
    let total = text.chars().count();
    if total <= chars {
        return text;
    }
    text.char_indices()
        .nth(total - chars)
        .map_or(text, |(i, _)| &text[i..])
}

/// `recall` within `max` characters: whole, cut, or spilled to a file in
/// `spill_dir` (named after `session`) when a directory is given.
#[must_use]
pub fn bounded(recall: &str, max: usize, spill_dir: Option<&Path>, session: &str) -> String {
    let total = recall.chars().count();
    if total <= max {
        return recall.to_owned();
    }
    if let Some(dir) = spill_dir {
        let file = dir.join(format!(
            "{session}-{}.txt",
            jiff::Timestamp::now().as_millisecond()
        ));
        match std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&file, recall)) {
            Ok(()) => {
                return format!(
                    "{}\n[... {} characters of provider recall; the full text is in {} (read it with read_file) ...]\n{}",
                    head(recall, PREVIEW_HEAD),
                    total.saturating_sub(PREVIEW_HEAD + PREVIEW_TAIL),
                    file.display(),
                    tail(recall, PREVIEW_TAIL),
                );
            }
            Err(err) => tracing::warn!("provider recall not spilled ({err}); cutting it instead"),
        }
    }
    format!(
        "{}\n[... provider recall truncated: {} more characters ...]",
        head(recall, max),
        total - max
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_recall_passes_whole_and_large_recall_is_cut() {
        assert_eq!(bounded("abc", 10, None, "s"), "abc");
        let cut = bounded(&"x".repeat(50), 10, None, "s");
        assert!(cut.starts_with(&"x".repeat(10)));
        assert!(cut.contains("40 more characters"), "{cut}");
    }
}
