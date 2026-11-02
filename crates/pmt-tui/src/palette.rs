//! The command palette (spec: tui, Fuzzy command palette): `Ctrl+P` or
//! `/palette` lists the commands executable here and the installed skills,
//! ranked by how well their name matches what is typed, each with its
//! description and disposition. Choosing one only puts it in the composer;
//! nothing runs until the person sends it.

use ratatui::text::{Line, Span};

use crate::theme::Theme;
use crate::wrap::truncate;

/// One command the palette offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The name without `/`.
    pub name: String,
    /// One line.
    pub description: String,
    /// Where it runs: `client`, `exec` or `skill`.
    pub disposition: String,
}

/// The open palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    /// What is typed.
    pub query: String,
    entries: Vec<Entry>,
    /// The highlighted row of [`Palette::ranked`].
    pub selected: usize,
}

impl Palette {
    /// A palette over `entries` with nothing typed.
    #[must_use]
    pub fn new(mut entries: Vec<Entry>) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries.dedup_by(|a, b| a.name == b.name);
        Self {
            query: String::new(),
            entries,
            selected: 0,
        }
    }

    /// Types `c`.
    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
    }

    /// Deletes the last typed character.
    pub fn pop(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    /// Moves the highlight `by` rows, staying on the list.
    pub fn move_selection(&mut self, by: isize) {
        let last = self.ranked().len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(by).min(last);
    }

    /// The entries matching the query, best first.
    #[must_use]
    pub fn ranked(&self) -> Vec<&Entry> {
        let mut scored: Vec<(i64, &Entry)> = self
            .entries
            .iter()
            .filter_map(|e| score(&self.query, &e.name).map(|s| (s, e)))
            .collect();
        scored.sort_by(|(a, ea), (b, eb)| b.cmp(a).then(ea.name.cmp(&eb.name)));
        scored.into_iter().map(|(_, e)| e).collect()
    }

    /// What `Enter` puts in the composer: `/<name> `.
    #[must_use]
    pub fn chosen(&self) -> Option<String> {
        self.ranked()
            .get(self.selected)
            .map(|e| format!("/{} ", e.name))
    }

    /// The palette's rows for `width` columns and at most `rows` lines.
    #[must_use]
    pub fn lines(&self, width: u16, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let width = usize::from(width).max(1);
        let mut lines = vec![Line::from(Span::styled(
            truncate(&format!("palette › {}", self.query), width),
            theme.card,
        ))];
        let ranked = self.ranked();
        if ranked.is_empty() {
            lines.push(Line::from("  no command matches"));
        }
        let shown = rows.saturating_sub(1).max(1);
        let first = (self.selected + 1).saturating_sub(shown);
        for (n, entry) in ranked.iter().enumerate().skip(first).take(shown) {
            let text = format!(
                "/{:<16} {}  [{}]",
                entry.name, entry.description, entry.disposition
            );
            let style = if n == self.selected {
                theme.selected
            } else {
                theme.frame
            };
            lines.push(Line::from(Span::styled(truncate(&text, width), style)));
        }
        lines
    }
}

/// How well `query` matches `name` as a case-insensitive subsequence:
/// `None` when it does not; higher for a prefix, consecutive characters
/// and an early first match, lower for a longer name.
#[must_use]
pub fn score(query: &str, name: &str) -> Option<i64> {
    let name: Vec<char> = name.to_lowercase().chars().collect();
    let mut score = 0i64;
    let mut at = 0usize;
    let mut previous: Option<usize> = None;
    for wanted in query.to_lowercase().chars() {
        let found = name[at..].iter().position(|c| *c == wanted)? + at;
        score += 10;
        if found == 0 {
            score += 15;
        }
        if previous.is_some_and(|p| p + 1 == found) {
            score += 8;
        }
        score -= i64::try_from(found - at).unwrap_or(i64::MAX).min(10);
        previous = Some(found);
        at = found + 1;
    }
    Some(score - i64::try_from(name.len()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::{Entry, Palette, score};

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.into(),
            description: format!("{name} things"),
            disposition: "exec".into(),
        }
    }

    /// Scenario: Insert, not run (the palette side).
    #[test]
    fn typing_cmpr_ranks_compress_first_and_enter_inserts_it() {
        let mut palette = Palette::new(
            ["compact", "compress", "model", "clear", "copy"]
                .into_iter()
                .map(entry)
                .collect(),
        );
        for c in "cmpr".chars() {
            palette.push(c);
        }
        let names: Vec<&str> = palette.ranked().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["compress"]);
        assert_eq!(palette.chosen().as_deref(), Some("/compress "));
    }

    #[test]
    fn closer_and_earlier_matches_rank_higher() {
        assert!(score("mod", "model") > score("mod", "memory-dump"));
        assert!(score("co", "copy") > score("co", "unicode"));
        assert_eq!(score("zz", "model"), None);
        assert!(score("", "anything").is_some(), "an empty query lists all");
        let mut palette = Palette::new(vec![entry("b"), entry("a")]);
        palette.move_selection(5);
        assert_eq!(palette.selected, 1, "stays on the list");
        palette.move_selection(-9);
        assert_eq!(palette.selected, 0);
    }
}
