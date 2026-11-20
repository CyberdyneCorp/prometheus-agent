//! Language syntax classes of a source file, for showing code coloured
//! (spec: tui, Inline diff review for file edits: the approval card's
//! "syntax-highlighted unified diff"). It reuses the tree-sitter grammars
//! the post-write check already carries (Rust, Python, JavaScript,
//! TypeScript, TSX), so highlighting costs no new dependency; files in
//! other languages get no classes and show uncoloured.

use std::ops::Range;
use std::path::Path;

/// What a run of source text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// `fn`, `def`, `return`, `self`...
    Keyword,
    /// A string or character literal.
    String,
    /// A comment.
    Comment,
    /// A number, boolean or null-like literal.
    Constant,
    /// A type name.
    Type,
}

/// The classed runs of one line: byte ranges within the line.
pub type LineClasses = Vec<(Range<usize>, Class)>;

/// The grammar for `path`'s extension, if it has one here.
fn language(path: &Path) -> Option<tree_sitter::Language> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "py" | "pyi" => tree_sitter_python::LANGUAGE.into(),
        "js" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" | "mts" | "cts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "jsx" | "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        _ => return None,
    })
}

/// Each line of `text` (as [`str::lines`] splits it) with its classed
/// runs; empty when `path`'s language is not known here. The grammars are
/// error tolerant, so a half-written file still gets most of its classes.
#[must_use]
pub fn classes(path: &Path, text: &str) -> Vec<LineClasses> {
    let Some(language) = language(path) else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(at, _)| at + 1))
        .collect();
    let mut lines = vec![LineClasses::new(); text.lines().count()];
    let mut cursor = tree.walk();
    'walk: loop {
        let node = cursor.node();
        match class(&node) {
            Some(class) => split(&starts, node.byte_range(), class, &mut lines),
            // A classed node is coloured whole; others show their parts.
            None if cursor.goto_first_child() => continue,
            None => {}
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    lines
}

/// The class of `node`, by its grammar kind (the grammars name these
/// alike, so one table serves all of them).
fn class(node: &tree_sitter::Node<'_>) -> Option<Class> {
    let kind = node.kind();
    if kind.contains("comment") {
        return Some(Class::Comment);
    }
    if !node.is_named() {
        // Anonymous word tokens are the language's keywords.
        let word = kind.len() > 1 && kind.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        return word.then_some(Class::Keyword);
    }
    match kind {
        "string"
        | "string_literal"
        | "raw_string_literal"
        | "char_literal"
        | "template_string"
        | "concatenated_string" => Some(Class::String),
        "integer_literal" | "float_literal" | "integer" | "float" | "number"
        | "boolean_literal" | "true" | "false" | "none" | "null" | "undefined" => {
            Some(Class::Constant)
        }
        "type_identifier" | "primitive_type" | "predefined_type" => Some(Class::Type),
        "self" | "this" | "super" | "crate" => Some(Class::Keyword),
        _ => None,
    }
}

/// Files the run `range` of `text` under the lines it covers.
fn split(starts: &[usize], range: Range<usize>, class: Class, lines: &mut [LineClasses]) {
    let first = starts
        .partition_point(|&s| s <= range.start)
        .saturating_sub(1);
    for (n, line) in lines.iter_mut().enumerate().skip(first) {
        let start = starts[n];
        if start >= range.end {
            break;
        }
        // The line's text ends before its newline.
        let end = starts.get(n + 1).map_or(usize::MAX, |next| next - 1);
        let from = range.start.max(start) - start;
        let to = range.end.min(end).saturating_sub(start);
        if from < to {
            line.push((from..to, class));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Class, classes};

    fn classed<'a>(
        line: &'a str,
        runs: &[(std::ops::Range<usize>, Class)],
    ) -> Vec<(&'a str, Class)> {
        runs.iter().map(|(r, c)| (&line[r.clone()], *c)).collect()
    }

    #[test]
    fn rust_keywords_strings_comments_numbers_and_types_are_classed() {
        let text = "// note\nfn main() -> u8 {\n    let s = \"hi\";\n    42\n}\n";
        let lines = classes(Path::new("src/main.rs"), text);
        let source: Vec<&str> = text.lines().collect();
        assert_eq!(classed(source[0], &lines[0]), [("// note", Class::Comment)]);
        assert_eq!(
            classed(source[1], &lines[1]),
            [("fn", Class::Keyword), ("u8", Class::Type)]
        );
        assert_eq!(
            classed(source[2], &lines[2]),
            [("let", Class::Keyword), ("\"hi\"", Class::String)]
        );
        assert_eq!(classed(source[3], &lines[3]), [("42", Class::Constant)]);
    }

    #[test]
    fn a_multi_line_string_is_classed_on_every_line_it_covers() {
        let text = "x = \"\"\"one\ntwo\"\"\"\n";
        let lines = classes(Path::new("a.py"), text);
        assert_eq!(lines[0], [(4..10, Class::String)]);
        assert_eq!(lines[1], [(0..6, Class::String)]);
    }

    #[test]
    fn unknown_languages_get_no_classes() {
        assert!(classes(Path::new("notes.txt"), "fn main() {}\n").is_empty());
    }
}
