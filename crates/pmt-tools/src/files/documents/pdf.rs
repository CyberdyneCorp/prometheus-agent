//! PDF text per page as Markdown, with the extraction coverage warning for
//! scanned documents.

use std::fmt::Write as _;

/// Characters of the text preceding an empty range shown as its label.
const LABEL_CHARS: usize = 80;

/// The pages as Markdown, prefixed with [`coverage_warning`] when too many
/// pages had no text.
pub(super) fn render(pages: &[String]) -> String {
    let mut text = coverage_warning(pages).unwrap_or_default();
    for (index, page) in pages.iter().enumerate() {
        let _ = write!(text, "<!-- page {} -->\n{}\n\n", index + 1, page.trim_end());
    }
    text
}

/// `[EXTRACTION COVERAGE WARNING: ...]` when more than 20% of the pages,
/// or at least 10 of them, yielded no text: each empty range with the last
/// text extracted before it.
#[must_use]
pub fn coverage_warning(pages: &[String]) -> Option<String> {
    let empty: Vec<usize> = pages
        .iter()
        .enumerate()
        .filter(|(_, page)| page.trim().is_empty())
        .map(|(index, _)| index)
        .collect();
    let total = pages.len();
    if empty.is_empty() || (empty.len() * 5 <= total && empty.len() < 10) {
        return None;
    }
    let mut warning = format!(
        "[EXTRACTION COVERAGE WARNING: {} of {total} pages in this PDF yielded no text (scanned images or no text layer); their content is missing from what follows.\n",
        empty.len()
    );
    for (first, last) in ranges(&empty) {
        let count = last - first + 1;
        let pages_label = if count == 1 {
            format!("page {} (1 page)", first + 1)
        } else {
            format!("pages {}-{} ({count} pages)", first + 1, last + 1)
        };
        let before = pages[..first]
            .iter()
            .rev()
            .find(|page| !page.trim().is_empty())
            .map(|page| tail(page.trim()));
        match before {
            Some(text) => {
                let _ = writeln!(warning, "  {pages_label}, after: \"{text}\"");
            }
            None => {
                let _ = writeln!(warning, "  {pages_label}, at the start of the document");
            }
        }
    }
    warning.push_str("]\n\n");
    Some(warning)
}

/// Consecutive runs of the sorted page `indexes`.
fn ranges(indexes: &[usize]) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &index in indexes {
        match ranges.last_mut() {
            Some((_, last)) if *last + 1 == index => *last = index,
            _ => ranges.push((index, index)),
        }
    }
    ranges
}

/// The last [`LABEL_CHARS`] characters of `text` on one line.
fn tail(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = flat.chars().count();
    let skip = count.saturating_sub(LABEL_CHARS);
    let kept: String = flat.chars().skip(skip).collect();
    if skip > 0 { format!("...{kept}") } else { kept }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(count: usize, empty: std::ops::RangeInclusive<usize>) -> Vec<String> {
        (1..=count)
            .map(|n| {
                if empty.contains(&n) {
                    String::new()
                } else {
                    format!("Text of page {n}.")
                }
            })
            .collect()
    }

    /// Scenario: Scanned PDF.
    #[test]
    fn a_scanned_range_is_reported_with_the_text_before_it() {
        let text = render(&pages(311, 42..=77));
        assert!(
            text.starts_with(
                "[EXTRACTION COVERAGE WARNING: 36 of 311 pages in this PDF yielded no text"
            ),
            "{}",
            &text[..200]
        );
        let warning = &text[..text.find("]\n\n").unwrap()];
        assert!(
            warning.contains("pages 42-77 (36 pages), after: \"Text of page 41.\""),
            "{warning}"
        );
        assert!(text.contains("<!-- page 78 -->\nText of page 78."));
    }

    #[test]
    fn a_few_empty_pages_are_not_worth_a_warning() {
        assert_eq!(coverage_warning(&pages(100, 5..=6)), None);
        // More than 20% of a short document is.
        let short = coverage_warning(&pages(4, 1..=1)).unwrap();
        assert!(
            short.contains("page 1 (1 page), at the start of the document"),
            "{short}"
        );
    }
}
