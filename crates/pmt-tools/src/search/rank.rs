//! BM25 ranking of deferred tools by their names and descriptions, over
//! lower-cased, lightly stemmed word tokens (`create_issue` and "Creates
//! issues" share `creat` and `issu`).

use std::collections::{BTreeMap, BTreeSet};

use super::Entry;

/// BM25 term-frequency saturation.
const K1: f64 = 1.2;
/// BM25 length normalization.
const B: f64 = 0.75;

/// The token index of one catalog.
pub(super) struct Index {
    documents: Vec<Vec<String>>,
    frequency: BTreeMap<String, usize>,
    average_len: f64,
}

impl Index {
    /// Indexes each entry's name and description.
    pub(super) fn new(entries: &[Entry]) -> Self {
        let documents: Vec<Vec<String>> = entries
            .iter()
            .map(|e| tokens(&format!("{} {}", e.name, e.description)))
            .collect();
        let mut frequency = BTreeMap::new();
        for document in &documents {
            for token in document.iter().collect::<BTreeSet<_>>() {
                *frequency.entry(token.clone()).or_insert(0) += 1;
            }
        }
        let total: usize = documents.iter().map(Vec::len).sum();
        let average_len = if documents.is_empty() {
            1.0
        } else {
            to_f64(total) / to_f64(documents.len())
        };
        Self {
            documents,
            frequency,
            average_len: average_len.max(1.0),
        }
    }

    /// The indices of the best `limit` documents for `query`, best first
    /// (ties keep catalog order); documents matching no token are left out.
    pub(super) fn rank(&self, query: &str, limit: usize) -> Vec<usize> {
        let terms: BTreeSet<String> = tokens(query).into_iter().collect();
        let mut scored: Vec<(usize, f64)> = self
            .documents
            .iter()
            .enumerate()
            .map(|(i, document)| (i, self.score(document, &terms)))
            .filter(|&(_, score)| score > 0.0)
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.into_iter().take(limit).map(|(i, _)| i).collect()
    }

    fn score(&self, document: &[String], terms: &BTreeSet<String>) -> f64 {
        let n = to_f64(self.documents.len());
        let len = to_f64(document.len());
        terms
            .iter()
            .map(|term| {
                let tf = to_f64(document.iter().filter(|t| *t == term).count());
                if tf == 0.0 {
                    return 0.0;
                }
                let df = to_f64(self.frequency.get(term).copied().unwrap_or(0));
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * len / self.average_len))
            })
            .sum()
    }
}

/// Lower-cased, stemmed word tokens of `text`: each run of letters and
/// digits, plus its camel-case parts when it has some (`GitHub` gives
/// `github`, `git` and `hub`).
pub(super) fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let parts = camel_parts(word);
        out.push(stem(&word.to_lowercase()));
        if parts.len() > 1 {
            out.extend(parts.iter().map(|p| stem(&p.to_lowercase())));
        }
    }
    out
}

/// `createIssue` -> `create`, `Issue`.
fn camel_parts(word: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut previous_lower = false;
    for (i, c) in word.char_indices() {
        if c.is_uppercase() && previous_lower {
            parts.push(&word[start..i]);
            start = i;
        }
        previous_lower = c.is_lowercase();
    }
    parts.push(&word[start..]);
    parts
}

/// Strips one common English inflection and then a final `e`, keeping at
/// least three characters (`issues` and `issue` -> `issu`, `creating` and
/// `create` -> `creat`).
fn stem(word: &str) -> String {
    const SUFFIXES: [&str; 4] = ["ing", "ed", "es", "s"];
    let long_enough = |stem: &&str| stem.chars().count() >= 3;
    let mut stem = SUFFIXES
        .iter()
        .find_map(|suffix| word.strip_suffix(suffix).filter(long_enough))
        .unwrap_or(word);
    if let Some(shorter) = stem.strip_suffix('e').filter(long_enough) {
        stem = shorter;
    }
    stem.to_owned()
}

fn to_f64(n: usize) -> f64 {
    u32::try_from(n).map_or(f64::from(u32::MAX), f64::from)
}

#[cfg(test)]
mod tests {
    use super::{Index, stem, tokens};
    use crate::search::Entry;

    fn entry(name: &str, description: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters: serde_json::json!({"type": "object"}),
            source: "s".to_owned(),
        }
    }

    #[test]
    fn names_split_on_case_and_separators_and_words_are_stemmed() {
        assert_eq!(
            tokens("mcp__gh__createIssue"),
            ["mcp", "gh", "createissu", "creat", "issu"]
        );
        assert_eq!(tokens("GitHub"), ["github", "git", "hub"]);
        assert_eq!(stem("issues"), "issu");
        assert_eq!(stem("issue"), "issu");
        assert_eq!(stem("files"), stem("file"));
        assert_eq!(stem("uses"), "use");
        assert_eq!(stem("list"), "list");
    }

    #[test]
    fn the_document_with_the_rarest_query_token_ranks_first() {
        let entries = [
            entry("list_files", "List the files of a repository"),
            entry("list_issues", "List the issues of a repository"),
            entry("read_file", "Read one file of a repository"),
        ];
        let index = Index::new(&entries);
        assert_eq!(index.rank("repository issues", 3)[0], 1);
        assert_eq!(index.rank("weather", 3), Vec::<usize>::new());
        assert_eq!(index.rank("repository", 2).len(), 2);
    }
}
