//! Whether `<file>::<fn>` names a test function: a line declaring
//! `fn <fn>(` whose attributes (the lines directly above it, past doc
//! comments) include a test attribute and no `#[ignore]`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Test attributes, by prefix.
const TEST_ATTRIBUTES: &[&str] = &["#[test]", "#[tokio::test"];

/// Source files read once each.
pub(crate) struct Sources {
    root: PathBuf,
    files: HashMap<String, Option<String>>,
}

impl Sources {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            files: HashMap::new(),
        }
    }

    /// `Ok` when `reference` (`<file>::<fn>`) names a runnable test.
    pub(crate) fn check(&mut self, reference: &str) -> Result<(), String> {
        let (file, name) = reference
            .rsplit_once("::")
            .ok_or_else(|| format!("`{reference}` is not `<file>::<fn>`"))?;
        let root = &self.root;
        let text = self
            .files
            .entry(file.to_owned())
            .or_insert_with(|| std::fs::read_to_string(root.join(file)).ok())
            .as_deref()
            .ok_or_else(|| format!("{file} does not exist"))?;
        match find(text, name) {
            Found::Test => Ok(()),
            Found::Ignored => Err(format!("{reference} is #[ignore]d")),
            Found::NotATest => Err(format!("{reference} has no test attribute")),
            Found::Missing => Err(format!("{file} has no fn {name}")),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Found {
    Test,
    Ignored,
    NotATest,
    Missing,
}

fn find(text: &str, name: &str) -> Found {
    let lines: Vec<&str> = text.lines().collect();
    let mut best = Found::Missing;
    for (at, line) in lines.iter().enumerate() {
        if !declares(line, name) {
            continue;
        }
        let found = classify(&lines[..at]);
        if found == Found::Test {
            return found;
        }
        best = found;
    }
    best
}

/// `[pub ][async ]fn <name>(` at the start of the trimmed line.
fn declares(line: &str, name: &str) -> bool {
    let mut rest = line.trim_start();
    for prefix in ["pub ", "async "] {
        rest = rest.strip_prefix(prefix).unwrap_or(rest);
    }
    rest.strip_prefix("fn ")
        .and_then(|rest| rest.strip_prefix(name))
        .is_some_and(|rest| rest.starts_with('(') || rest.starts_with('<'))
}

/// The attributes directly above a declaration (doc comments and
/// comments may sit between them).
fn classify(above: &[&str]) -> Found {
    let attributes: Vec<&str> = above
        .iter()
        .rev()
        .map(|line| line.trim())
        .take_while(|line| line.starts_with("#[") || line.starts_with("//"))
        .filter(|line| line.starts_with("#["))
        .collect();
    if attributes.iter().any(|a| a.starts_with("#[ignore")) {
        return Found::Ignored;
    }
    if attributes
        .iter()
        .any(|a| TEST_ATTRIBUTES.iter().any(|t| a.starts_with(t)))
    {
        Found::Test
    } else {
        Found::NotATest
    }
}

#[cfg(test)]
mod tests {
    use super::{Found, find};

    #[test]
    fn test_functions_are_told_from_helpers_and_ignored_tests() {
        let text = "\
#[test]
fn plain() {}

/// Doc.
#[tokio::test(flavor = \"multi_thread\")]
// a comment
async fn asynchronous() {}

#[test]
#[ignore = \"slow\"]
fn skipped() {}

fn helper() {}

#[cfg(test)]
mod inner {
    #[test]
    pub fn nested() {}
}
";
        assert_eq!(find(text, "plain"), Found::Test);
        assert_eq!(find(text, "asynchronous"), Found::Test);
        assert_eq!(find(text, "nested"), Found::Test);
        assert_eq!(find(text, "skipped"), Found::Ignored);
        assert_eq!(find(text, "helper"), Found::NotATest);
        assert_eq!(find(text, "plai"), Found::Missing);
    }
}
