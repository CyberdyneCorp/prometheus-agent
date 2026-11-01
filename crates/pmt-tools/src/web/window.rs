//! The deterministic head/tail window of a long extracted page (spec:
//! web-and-media-tools, `web_extract` tool contract).
//!
//! A page over budget keeps about 75% head and 25% tail, both cut on line
//! boundaries, so the same page always yields the same bytes. The omitted
//! middle stays reachable through the stored full text.

use std::path::Path;

/// Characters the window reserves for the omission marker and footer.
const FRAME: usize = 600;

/// `text` within `budget` characters: whole when it fits, else the head and
/// tail window with a `[TRUNCATED]` footer naming `stored` and the
/// `read_file` call that pages through the omitted lines.
#[must_use]
pub fn window(text: &str, budget: usize, stored: &Path) -> String {
    if text.chars().count() <= budget {
        return text.to_owned();
    }
    let room = budget.saturating_sub(FRAME).max(budget / 2);
    let head_room = room * 3 / 4;
    let head = head_part(text, head_room);
    let tail = tail_part(&text[head.len()..], room - head_room);
    let middle = &text[head.len()..text.len() - tail.len()];
    let first_omitted = head.matches('\n').count() + 1;
    let omitted_lines = middle.lines().count().max(1);
    let shown = head.chars().count() + tail.chars().count();
    let total = text.chars().count();
    let path = stored.display();
    let (head, tail) = (with_newline(head), with_newline(tail));
    format!(
        "{head}[... {omitted_lines} lines omitted ...]\n{tail}[TRUNCATED] showing {shown} of \
         {total} characters (head and tail). Full text: {path}. Read the omitted middle with \
         read_file(path=\"{path}\", offset={first_omitted}, limit={omitted_lines})"
    )
}

/// The longest prefix of whole lines within `room` characters (a single
/// over-long first line is cut by characters).
fn head_part(text: &str, room: usize) -> &str {
    let end = byte_index(text, room);
    let prefix = &text[..end];
    match prefix.rfind('\n') {
        Some(at) => &text[..=at],
        None => prefix,
    }
}

/// The longest suffix of whole lines within `room` characters.
fn tail_part(text: &str, room: usize) -> &str {
    let chars = text.chars().count();
    let start = byte_index(text, chars.saturating_sub(room));
    let suffix = &text[start..];
    if start == 0 || text[..start].ends_with('\n') {
        return suffix;
    }
    match suffix.find('\n') {
        Some(at) => &suffix[at + 1..],
        None => suffix,
    }
}

/// `part` ending with a line break.
fn with_newline(part: &str) -> std::borrow::Cow<'_, str> {
    if part.ends_with('\n') {
        part.into()
    } else {
        format!("{part}\n").into()
    }
}

/// The byte offset of character `n` (or the end).
fn byte_index(text: &str, n: usize) -> usize {
    text.char_indices().nth(n).map_or(text.len(), |(at, _)| at)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::Path;

    use super::window;

    fn page(lines: usize) -> String {
        (1..=lines).fold(String::new(), |mut page, n| {
            let _ = writeln!(page, "line {n:05} of the page");
            page
        })
    }

    #[test]
    fn a_page_within_budget_is_whole() {
        let text = page(10);
        assert_eq!(window(&text, 15_000, Path::new("/x")), text);
    }

    #[test]
    fn a_long_page_keeps_head_and_tail_on_line_boundaries() {
        let text = page(3_000); // 72,000 characters
        let out = window(&text, 15_000, Path::new("/cache/web/abc.md"));
        let chars = out.chars().count();
        assert!((14_000..=15_000).contains(&chars), "{chars}");
        assert!(out.starts_with("line 00001 of the page\n"));
        let (body, footer) = out.rsplit_once("\n[TRUNCATED]").unwrap();
        assert!(body.trim_end().ends_with("line 03000 of the page"));
        assert!(footer.contains("/cache/web/abc.md"));
        // Head is about three times the tail.
        let (head, tail) = body.split_once("lines omitted ...]\n").unwrap();
        assert!(
            (tail.len() * 5 / 2..tail.len() * 7 / 2).contains(&head.len()),
            "{} vs {}",
            head.len(),
            tail.len()
        );
        // The read_file call names the first omitted line.
        let first = head.matches("of the page").count() + 1;
        assert!(footer.contains(&format!("offset={first},")), "{footer}");
        assert!(
            body.lines()
                .all(|l| l.starts_with("line ") || l.starts_with("[..."))
        );
    }

    #[test]
    fn one_huge_line_is_cut_by_characters() {
        let text = "x".repeat(50_000);
        let out = window(&text, 2_000, Path::new("/p"));
        assert!(out.chars().count() <= 2_400, "{}", out.chars().count());
        assert!(out.contains("[TRUNCATED]"));
    }
}
