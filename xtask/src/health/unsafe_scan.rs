//! Finds every use of the `unsafe` keyword in one source file.
//!
//! The scan runs over tokens rather than the syntax tree so that `unsafe`
//! inside macro invocations is seen too. Comments are not tokens, so the
//! `// SAFETY:` check reads the source lines around each finding.

use std::str::FromStr;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

/// One `unsafe` keyword.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UnsafeUse {
    /// 1-based line of the keyword.
    pub(crate) line: usize,
    /// An `unsafe { }` block or `unsafe impl` asserts an invariant at this
    /// spot and must say why it holds; `unsafe fn`, `unsafe trait` and
    /// `unsafe extern` declare an obligation, documented under `# Safety`.
    pub(crate) needs_safety_comment: bool,
    pub(crate) has_safety_comment: bool,
}

/// Every `unsafe` keyword in `source`, in order.
pub(crate) fn scan(source: &str) -> Result<Vec<UnsafeUse>, String> {
    let tokens = TokenStream::from_str(source).map_err(|e| e.to_string())?;
    let mut lines = Vec::new();
    find(tokens, &mut lines);
    let source_lines: Vec<&str> = source.lines().collect();
    Ok(lines
        .into_iter()
        .map(|(line, needs_safety_comment)| UnsafeUse {
            line,
            needs_safety_comment,
            has_safety_comment: has_safety_comment(&source_lines, line),
        })
        .collect())
}

/// Collects `(line, asserts_invariant)` for each `unsafe` token.
fn find(tokens: TokenStream, found: &mut Vec<(usize, bool)>) {
    let mut iter = tokens.into_iter().peekable();
    while let Some(token) = iter.next() {
        match token {
            TokenTree::Ident(ident) if ident == "unsafe" => {
                found.push((ident.span().start().line, asserts_invariant(iter.peek())));
            }
            TokenTree::Group(group) => find(group.stream(), found),
            _ => {}
        }
    }
}

fn asserts_invariant(next: Option<&TokenTree>) -> bool {
    match next {
        Some(TokenTree::Group(group)) => group.delimiter() == Delimiter::Brace,
        Some(TokenTree::Ident(ident)) => ident == "impl",
        _ => false,
    }
}

/// True when the keyword's line, or the run of comment and attribute lines
/// directly above it, carries a `// SAFETY:` comment.
fn has_safety_comment(lines: &[&str], line: usize) -> bool {
    let is_safety = |text: &str| text.contains("// SAFETY:");
    let Some(own) = lines.get(line - 1) else {
        return false;
    };
    is_safety(own)
        || lines[..line - 1]
            .iter()
            .rev()
            .map(|l| l.trim_start())
            .take_while(|l| l.starts_with("//") || l.starts_with("#["))
            .any(is_safety)
}

#[cfg(test)]
mod tests {
    use super::{UnsafeUse, scan};

    fn uses(src: &str) -> Vec<(usize, bool, bool)> {
        scan(src)
            .unwrap()
            .into_iter()
            .map(
                |UnsafeUse {
                     line,
                     needs_safety_comment,
                     has_safety_comment,
                 }| { (line, needs_safety_comment, has_safety_comment) },
            )
            .collect()
    }

    #[test]
    fn block_needs_a_comment_directly_above() {
        let src =
            "fn f() {\n    // SAFETY: p is valid.\n    unsafe { g() };\n\n    unsafe { g() };\n}";
        assert_eq!(uses(src), [(3, true, true), (5, true, false)]);
    }

    #[test]
    fn a_blank_line_breaks_the_comment_run() {
        let src = "// SAFETY: stale.\n\nfn f() { unsafe { g() } }";
        assert_eq!(uses(src), [(3, true, false)]);
    }

    #[test]
    fn declarations_do_not_need_a_comment() {
        let src =
            "/// # Safety\n/// p must be valid.\nunsafe fn f() {}\nunsafe extern \"C\" { fn g(); }";
        assert_eq!(uses(src), [(3, false, false), (4, false, false)]);
    }

    #[test]
    fn unsafe_inside_macros_and_impls_is_found() {
        let src = "// SAFETY: S owns no thread-bound state.\nunsafe impl Send for S {}\nfn f() { vec![unsafe { g() }]; }";
        assert_eq!(uses(src), [(2, true, true), (3, true, false)]);
    }

    #[test]
    fn the_word_in_strings_and_comments_is_not_unsafe_code() {
        assert!(uses("// unsafe { }\nconst S: &str = \"unsafe { }\";").is_empty());
    }
}
