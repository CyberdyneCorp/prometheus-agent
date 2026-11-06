//! The summary budget (spec: subagent-delegation, Summary budget protects
//! the parent context): each result's summary inserted into the parent's
//! context is capped at the smaller of `delegation.max_summary_chars` and
//! half the parent's remaining context window divided across the call's
//! tasks (never below [`FLOOR_CHARS`]). A capped summary keeps its first
//! 75% and last 25% around a marker, and the full text is written where
//! the parent can `read_file` it (`summary_ref`), so nothing is lost.

use std::path::{Path, PathBuf};

/// The marker between the kept head and tail.
pub const MARKER: &str = "\n[SUMMARY TRUNCATED]\n";
/// The smallest context-derived cap.
pub const FLOOR_CHARS: usize = 2_000;
/// Characters per token when a token window is turned into characters.
pub const CHARS_PER_TOKEN: u64 = 4;

/// The cap for one of `tasks` summaries: `max_summary_chars` (`0`: no
/// configured cap) against half of `remaining_tokens` of context.
#[must_use]
pub fn cap(max_summary_chars: u64, remaining_tokens: Option<u64>, tasks: usize) -> Option<usize> {
    let configured = usize::try_from(max_summary_chars).ok().filter(|&c| c > 0);
    let from_context = remaining_tokens.map(|tokens| {
        let chars = tokens.saturating_mul(CHARS_PER_TOKEN) / 2;
        let per_task = chars / u64::try_from(tasks.max(1)).unwrap_or(1);
        usize::try_from(per_task)
            .unwrap_or(usize::MAX)
            .max(FLOOR_CHARS)
    });
    match (configured, from_context) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// `summary` within `cap` characters: unchanged when it fits, else its
/// first 75% and last 25% of the allowed length around [`MARKER`].
#[must_use]
pub fn truncate(summary: &str, cap: usize) -> Option<String> {
    let chars = summary.chars().count();
    if chars <= cap {
        return None;
    }
    let room = cap.saturating_sub(MARKER.chars().count());
    let head = room * 3 / 4;
    let tail = room - head;
    let start: String = summary.chars().take(head).collect();
    let end: String = summary.chars().skip(chars - tail).collect();
    Some(format!("{start}{MARKER}{end}"))
}

/// Writes the full `summary` of child `subagent_id` under `dir` (the
/// parent session's tool-output directory, which its `read_file` may
/// read) and returns the path.
///
/// # Errors
///
/// The file could not be written.
pub fn keep_full(dir: &Path, subagent_id: &str, summary: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{subagent_id}-summary.md"));
    std::fs::write(&path, summary)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Oversized summary is kept losslessly.
    #[test]
    fn an_oversized_summary_is_capped_and_kept_in_full() {
        let summary: String = (0..100_000)
            .map(|i| char::from(b'a' + u8::try_from(i % 26).unwrap()))
            .collect();
        let cap = cap(24_000, None, 1).unwrap();
        let inserted = truncate(&summary, cap).unwrap();
        assert!(inserted.chars().count() <= 24_000);
        assert!(inserted.contains("[SUMMARY TRUNCATED]"));
        assert!(inserted.starts_with(&summary[..1000]));
        assert!(inserted.ends_with(&summary[summary.len() - 1000..]));
        let dir = tempfile::tempdir().unwrap();
        let path = keep_full(dir.path(), "sa-x", &summary).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap().len(), 100_000);
    }

    #[test]
    fn the_context_share_and_floor_apply() {
        assert_eq!(cap(24_000, Some(8_000), 4), Some(4_000));
        assert_eq!(cap(24_000, Some(100), 4), Some(2_000));
        assert_eq!(cap(0, None, 2), None);
        assert_eq!(cap(0, Some(1_000_000), 1), Some(2_000_000));
        assert!(truncate("short", 2_000).is_none());
    }
}
