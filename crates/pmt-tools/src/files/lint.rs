//! Post-write syntax checks (spec: execution-environments, Post-write
//! diagnostics).
//!
//! After `write_file` and `patch` the new content is parsed in-process for
//! its language: Rust, Python, JavaScript and TypeScript through
//! tree-sitter grammars, JSON, TOML and YAML through their parsers. The
//! check never fails the write: a missing checker, a checker that panics or
//! runs past its budget all report `status = "skipped"`. Semantic
//! diagnostics from language servers (`lsp_diagnostics`) are the M2
//! `lsp-diagnostics` capability.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use serde::Serialize;

/// How long one check may take before it is reported as skipped.
pub const BUDGET: Duration = Duration::from_secs(2);

/// Files larger than this are not checked.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

/// A syntax checker: `Err` carries the parser's message with its line.
pub type Checker = fn(&str) -> Result<(), String>;

/// The verdict attached to the tool result as `lint`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Lint {
    /// `ok`, `error` or `skipped`.
    pub status: LintStatus,
    /// The parser message (`error`) or why the check was skipped.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub output: String,
}

/// The outcome of a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LintStatus {
    /// The syntax is valid.
    Ok,
    /// The parser reported an error.
    Error,
    /// No check ran or it failed.
    Skipped,
}

impl Lint {
    fn skipped(why: impl Into<String>) -> Self {
        Self {
            status: LintStatus::Skipped,
            output: why.into(),
        }
    }
}

/// The checkers by file extension.
#[derive(Debug, Clone)]
pub struct Linter {
    checkers: Vec<(&'static str, Checker)>,
    budget: Duration,
}

impl Default for Linter {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Linter {
    /// The built-in checkers.
    #[must_use]
    pub fn builtin() -> Self {
        let checkers: Vec<(&'static str, Checker)> = vec![
            ("rs", check_rust),
            ("py", check_python),
            ("pyi", check_python),
            ("js", check_javascript),
            ("mjs", check_javascript),
            ("cjs", check_javascript),
            ("jsx", check_tsx),
            ("ts", check_typescript),
            ("mts", check_typescript),
            ("cts", check_typescript),
            ("tsx", check_tsx),
            ("json", check_json),
            ("toml", check_toml),
            ("yaml", check_yaml),
            ("yml", check_yaml),
        ];
        Self {
            checkers,
            budget: BUDGET,
        }
    }

    /// Replaces (or adds) the checker of `extension`.
    #[must_use]
    pub fn with(mut self, extension: &'static str, checker: Checker) -> Self {
        self.checkers.retain(|(ext, _)| *ext != extension);
        self.checkers.push((extension, checker));
        self
    }

    /// Sets the time budget of one check.
    #[must_use]
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    /// Checks `content` written to `path`.
    #[must_use]
    pub fn check(&self, path: &Path, content: &str) -> Lint {
        let extension = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let Some(checker) = self
            .checkers
            .iter()
            .find(|(ext, _)| *ext == extension)
            .map(|(_, checker)| *checker)
        else {
            return Lint::skipped("no syntax checker for this file type");
        };
        if content.len() > MAX_BYTES {
            return Lint::skipped("file too large to check");
        }
        self.run(checker, content.to_owned())
    }

    /// Runs `checker` on its own thread so a slow parse cannot hold the
    /// tool call past the budget, and a panic is contained.
    fn run(&self, checker: Checker, content: String) -> Lint {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("pmt-lint".into())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| checker(&content)));
                let _ = tx.send(outcome);
            });
        if spawned.is_err() {
            return Lint::skipped("the syntax checker could not start");
        }
        match rx.recv_timeout(self.budget) {
            Ok(Ok(Ok(()))) => Lint {
                status: LintStatus::Ok,
                output: String::new(),
            },
            Ok(Ok(Err(message))) => Lint {
                status: LintStatus::Error,
                output: message,
            },
            Ok(Err(_)) => Lint::skipped("the syntax checker failed"),
            Err(_) => Lint::skipped("the syntax checker ran out of time"),
        }
    }
}

fn check_json(content: &str) -> Result<(), String> {
    serde_json::from_str::<serde_json::Value>(content)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn check_toml(content: &str) -> Result<(), String> {
    toml::from_str::<toml::Table>(content)
        .map(|_| ())
        .map_err(|err| err.to_string().trim_end().to_owned())
}

fn check_yaml(content: &str) -> Result<(), String> {
    for event in saphyr_parser::Parser::new_from_str(content) {
        if let Err(err) = event {
            let mark = err.marker();
            return Err(format!(
                "line {}, column {}: {}",
                mark.line(),
                mark.col() + 1,
                err.info()
            ));
        }
    }
    Ok(())
}

fn check_rust(content: &str) -> Result<(), String> {
    tree_sitter_check(content, &tree_sitter_rust::LANGUAGE.into())
}

fn check_python(content: &str) -> Result<(), String> {
    tree_sitter_check(content, &tree_sitter_python::LANGUAGE.into())
}

fn check_javascript(content: &str) -> Result<(), String> {
    tree_sitter_check(content, &tree_sitter_javascript::LANGUAGE.into())
}

fn check_typescript(content: &str) -> Result<(), String> {
    tree_sitter_check(content, &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
}

fn check_tsx(content: &str) -> Result<(), String> {
    tree_sitter_check(content, &tree_sitter_typescript::LANGUAGE_TSX.into())
}

/// Parses with a tree-sitter grammar and reports the first error or
/// missing node in document order.
fn tree_sitter_check(content: &str, language: &tree_sitter::Language) -> Result<(), String> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(language)
        .map_err(|err| format!("grammar unavailable: {err}"))?;
    let tree = parser
        .parse(content, None)
        .ok_or_else(|| "the parser gave no result".to_owned())?;
    let root = tree.root_node();
    if !root.has_error() {
        return Ok(());
    }
    let node = first_error(root).unwrap_or(root);
    let at = node.start_position();
    let what = if node.is_missing() {
        format!("missing {}", node.kind())
    } else {
        let snippet: String = content
            .get(node.byte_range())
            .unwrap_or_default()
            .chars()
            .take(40)
            .collect();
        format!("syntax error near `{}`", snippet.trim())
    };
    Err(format!(
        "line {}, column {}: {what}",
        at.row + 1,
        at.column + 1
    ))
}

fn first_error(root: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            return Some(node);
        }
        // Descend only into subtrees that contain the error.
        if node.has_error() && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::{LintStatus, Linter};

    fn check(name: &str, content: &str) -> (LintStatus, String) {
        let lint = Linter::builtin().check(Path::new(name), content);
        (lint.status, lint.output)
    }

    #[test]
    fn every_language_accepts_valid_and_reports_invalid_syntax() {
        let cases = [
            (
                "a.rs",
                "fn main() { let x = 1; }\n",
                "fn main() { let x = ; }\n",
            ),
            (
                "a.py",
                "def f(x):\n    return x\n",
                "def f(x)\n    return x\n",
            ),
            ("a.js", "const a = [1, 2];\n", "const a = [1, 2;\n"),
            ("a.ts", "let n: number = 1;\n", "let n: number = ;\n"),
            (
                "a.tsx",
                "const e = <div>{1}</div>;\n",
                "const e = <div>{1</div>;\n",
            ),
            ("a.json", "{\"a\": [1, 2]}", "{\"a\": [1, 2],}"),
            ("a.toml", "a = 1\n[b]\nc = \"x\"\n", "a = \n"),
            ("a.yaml", "a: 1\nb:\n  - x\n", "a: [1, 2\nb: 3\n"),
        ];
        for (name, good, bad) in cases {
            assert_eq!(check(name, good), (LintStatus::Ok, String::new()), "{name}");
            let (status, output) = check(name, bad);
            assert_eq!(status, LintStatus::Error, "{name}: {output}");
            assert!(output.contains("line"), "{name}: {output}");
        }
    }

    #[test]
    fn json_trailing_comma_names_the_line() {
        let (status, output) = check("x.json", "{\n  \"a\": 1,\n}\n");
        assert_eq!(status, LintStatus::Error);
        assert!(output.contains("trailing comma at line 3"), "{output}");
    }

    #[test]
    fn unknown_types_are_skipped() {
        assert_eq!(check("notes.md", "# x").0, LintStatus::Skipped);
    }

    #[test]
    fn a_panicking_or_slow_checker_is_skipped() {
        let panicking = Linter::builtin().with("yaml", |_| panic!("checker bug"));
        let lint = panicking.check(Path::new("a.yaml"), "a: 1\n");
        assert_eq!(lint.status, LintStatus::Skipped);
        assert_eq!(lint.output, "the syntax checker failed");
        let slow = Linter::builtin()
            .with("toml", |_| {
                std::thread::sleep(Duration::from_millis(500));
                Ok(())
            })
            .with_budget(Duration::from_millis(20));
        assert_eq!(
            slow.check(Path::new("a.toml"), "").status,
            LintStatus::Skipped
        );
    }
}
