//! Tool names in documentation come from the registry (spec:
//! agent-turn-loop, Generated tool names in documentation).
//!
//! A page names a tool by linking to its entry in the generated tools
//! reference (`[`todo`](../reference/tools.md#todo)`). Every such link must
//! target a listed tool and its text must be that name, so a hand-written
//! `todo_list` reference fails `cargo xtask gen` and `gen --check`.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::artifacts::TOOLS_REFERENCE;

static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\(([^)\s#]*)#([^)\s]*)\)").expect("valid regex"));

/// Checks every Markdown page under `docs/`.
pub(crate) fn check(root: &Path) -> Result<(), String> {
    let known: BTreeSet<String> = pmt_tools::toolset::builtin_tool_names()
        .into_iter()
        .collect();
    let mut problems = Vec::new();
    for page in pages(&root.join("docs")) {
        let text =
            std::fs::read_to_string(&page).map_err(|e| format!("{}: {e}", page.display()))?;
        let relative = page.strip_prefix(root).unwrap_or(&page).to_path_buf();
        problems.extend(scan(&relative, &text, &known));
    }
    if problems.is_empty() {
        return Ok(());
    }
    for problem in &problems {
        eprintln!("{problem}");
    }
    Err(format!(
        "{} tool reference(s) name no registered tool; link to an entry of {TOOLS_REFERENCE}",
        problems.len()
    ))
}

fn pages(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.filter_map(Result::ok).map(|e| e.path()) {
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// The problems of `page` (a path relative to the repository root).
pub(crate) fn scan(page: &Path, text: &str, known: &BTreeSet<String>) -> Vec<String> {
    let dir = page.parent().unwrap_or(Path::new(""));
    let mut problems = Vec::new();
    for (number, line) in text.lines().enumerate() {
        for link in LINK.captures_iter(line) {
            let target = if link[2].is_empty() {
                page.to_path_buf()
            } else {
                normalize(&dir.join(&link[2]))
            };
            if target != Path::new(TOOLS_REFERENCE) {
                continue;
            }
            let (label, anchor) = (link[1].trim_matches('`'), &link[3]);
            let at = format!("{}:{}", page.display(), number + 1);
            if !known.contains(anchor) {
                problems.push(format!("{at}: unknown tool '{anchor}'"));
            } else if label != anchor {
                problems.push(format!("{at}: link text '{label}' names tool '{anchor}'"));
            }
        }
    }
    problems
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::scan;

    fn known() -> BTreeSet<String> {
        pmt_tools::toolset::builtin_tool_names()
            .into_iter()
            .collect()
    }

    #[test]
    fn renamed_tool_fails_the_check() {
        let page = Path::new("docs/guide/agent.md");
        let good = "Track steps with [`todo`](../reference/tools.md#todo).";
        assert!(scan(page, good, &known()).is_empty());
        let bad = "Track steps with [`todo_list`](../reference/tools.md#todo_list).";
        assert_eq!(
            scan(page, bad, &known()),
            ["docs/guide/agent.md:1: unknown tool 'todo_list'"]
        );
        let mislabeled = "[`todo_list`](../reference/tools.md#todo)";
        assert_eq!(
            scan(page, mislabeled, &known()),
            ["docs/guide/agent.md:1: link text 'todo_list' names tool 'todo'"]
        );
    }

    #[test]
    fn only_links_to_the_tools_reference_are_checked() {
        let page = Path::new("docs/guide/agent.md");
        // guide/tools.md is the guide, not the reference.
        assert!(scan(page, "[x](tools.md#choosing-toolsets)", &known()).is_empty());
        let reference = Path::new("docs/reference/cli.md");
        assert_eq!(
            scan(reference, "[`nope`](tools.md#nope)", &known()).len(),
            1
        );
    }

    #[test]
    fn the_repository_docs_pass() {
        assert!(super::check(&crate::repo_root()).is_ok());
    }
}
