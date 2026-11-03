//! The deferred-tool manifest embedded in the `tool_search` description:
//! every deferred tool with the first sentence of its description, grouped
//! by server or toolset, within the listing budget. Over budget, the
//! largest group degrades first, to its tool names and then to one line
//! with its tool count, so one huge server cannot crowd out small ones.

use std::fmt::Write as _;

use super::Entry;

/// The longest description excerpt per tool.
const EXCERPT_CHARS: usize = 60;

/// How one group is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    /// One line per tool with its excerpt.
    Full,
    /// The tool names on one line.
    Names,
    /// The tool count.
    Count,
}

struct Group<'a> {
    source: &'a str,
    entries: Vec<&'a Entry>,
    level: Level,
}

impl Group<'_> {
    fn render(&self) -> String {
        match self.level {
            Level::Full => {
                let mut text = format!("{}:", self.source);
                for entry in &self.entries {
                    let _ = write!(text, "\n- {}: {}", entry.name, excerpt(&entry.description));
                }
                text
            }
            Level::Names => {
                let names: Vec<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
                format!("{}: {}", self.source, names.join(", "))
            }
            Level::Count => format!("{}: {} tools", self.source, self.entries.len()),
        }
    }
}

/// The manifest of `entries` within `budget` characters (as small as it
/// gets when even one line per group is over budget).
pub(super) fn render(entries: &[Entry], budget: u64) -> String {
    let mut groups: Vec<Group<'_>> = Vec::new();
    for entry in entries {
        match groups.iter_mut().find(|g| g.source == entry.source) {
            Some(group) => group.entries.push(entry),
            None => groups.push(Group {
                source: &entry.source,
                entries: vec![entry],
                level: Level::Full,
            }),
        }
    }
    loop {
        let rendered: Vec<String> = groups.iter().map(Group::render).collect();
        let text = rendered.join("\n");
        if u64::try_from(text.chars().count()).unwrap_or(u64::MAX) <= budget {
            return text;
        }
        let largest = rendered
            .iter()
            .enumerate()
            .filter(|(i, _)| groups[*i].level != Level::Count)
            .max_by_key(|(i, r)| (r.chars().count(), std::cmp::Reverse(*i)))
            .map(|(i, _)| i);
        let Some(i) = largest else {
            return text;
        };
        groups[i].level = match groups[i].level {
            Level::Full => Level::Names,
            Level::Names | Level::Count => Level::Count,
        };
    }
}

/// The first sentence of `description`, at most [`EXCERPT_CHARS`]
/// characters.
fn excerpt(description: &str) -> String {
    let first = description
        .split_inclusive(['.', '\n'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches('.');
    let mut out: String = first.chars().take(EXCERPT_CHARS).collect();
    if first.chars().count() > EXCERPT_CHARS {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{excerpt, render};
    use crate::search::Entry;

    fn entries(source: &str, count: usize) -> Vec<Entry> {
        (0..count)
            .map(|i| Entry {
                name: format!("mcp__{source}__tool_{i}"),
                description: format!("Does thing number {i} for the {source} server. More."),
                parameters: serde_json::json!({"type": "object"}),
                source: format!("mcp-{source}"),
            })
            .collect()
    }

    #[test]
    fn an_oversized_server_collapses_alone() {
        let mut all = entries("huge", 3_000);
        all.extend(entries("small", 12));
        let text = render(&all, 4_000 * 4);
        assert!(text.contains("mcp-huge: 3000 tools"), "{text}");
        assert!(
            text.contains("- mcp__small__tool_11: Does thing number 11 for the small server"),
            "{text}"
        );
        assert!(text.chars().count() <= 16_000);
    }

    #[test]
    fn excerpts_keep_the_first_sentence_within_sixty_characters() {
        assert_eq!(
            excerpt("Lists issues. Pagination supported."),
            "Lists issues"
        );
        assert_eq!(excerpt(&"x".repeat(70)).chars().count(), 61);
    }
}
