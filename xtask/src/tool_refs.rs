//! Tool names in documentation come from the registry (spec:
//! agent-turn-loop, Generated tool names in documentation).
//!
//! A hand-written page names a tool by linking to its entry in the
//! generated tools reference (`[`todo`](../reference/tools.md#todo)`).
//! The check, run by `cargo xtask gen` and `gen --check`, fails on:
//!
//! - a link to the reference that names no registered tool, or whose text
//!   is not the tool's name;
//! - an inline-code name in a tool context (``the `x` tool``, ``tool `x` ``)
//!   that is not a registered tool, and an inline-code name that extends a
//!   registered one (`todo_list` for `todo`) wherever it appears;
//! - on hand-written pages, a registered tool named in inline code without
//!   the link. A name that is also a toolset or a configuration key
//!   (`terminal`, `memory`) is read as a tool only in a tool context.
//!
//! Generated pages (the reference, which includes the CLI help) cannot
//! link, so only the unknown-name rules apply to them. A page that
//! documents another product's tools opts out with [`OPT_OUT`].

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::artifacts::{Artifact, REFERENCE_DIR, TOOLS_REFERENCE};

/// Starts the comment (`<!-- tool-refs: off -- <why> -->`) that marks a
/// page whose inline-code names are not Prometheus tools.
pub(crate) const OPT_OUT: &str = "<!-- tool-refs: off";

static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\(([^)\s#]*)#([^)\s]*)\)").expect("valid regex"));
static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").expect("valid regex"));
static IDENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]*$").expect("valid regex"));
static TOOL_AFTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s+tools?\b").expect("valid regex"));
static TOOL_BEFORE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\btools?\s+$").expect("valid regex"));

/// The names the check knows.
pub(crate) struct Names {
    /// Registered tool names.
    tools: BTreeSet<String>,
    /// Names that also mean something else (toolsets, config key segments).
    shared: BTreeSet<String>,
}

impl Names {
    /// The built-in tools, toolsets and configuration keys.
    pub(crate) fn builtin() -> Self {
        let tools = pmt_tools::toolset::builtin_tool_names()
            .into_iter()
            .collect();
        let toolsets = pmt_tools::toolset::builtin().into_iter().map(|t| t.name);
        let keys = pmt_config::catalog::keys()
            .into_iter()
            .flat_map(|k| k.pattern.split('.').map(str::to_owned).collect::<Vec<_>>());
        Self {
            tools,
            shared: toolsets.chain(keys).collect(),
        }
    }

    /// The registered tool `name` extends (`todo` for `todo_list`).
    fn extended(&self, name: &str) -> Option<&str> {
        self.tools
            .iter()
            .find(|tool| {
                name.strip_prefix(tool.as_str())
                    .is_some_and(|rest| rest.starts_with('_'))
            })
            .map(String::as_str)
    }
}

/// Whether a page is written by hand or generated from the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    HandWritten,
    Generated,
}

/// Checks every hand-written Markdown page under `docs/` and the generated
/// pages as `artifacts` renders them.
pub(crate) fn check(root: &Path, artifacts: &[Artifact]) -> Result<(), String> {
    let names = Names::builtin();
    let mut problems = Vec::new();
    for page in pages(&root.join("docs")) {
        let relative = page.strip_prefix(root).unwrap_or(&page).to_path_buf();
        if relative.starts_with(REFERENCE_DIR) {
            continue;
        }
        let text =
            std::fs::read_to_string(&page).map_err(|e| format!("{}: {e}", page.display()))?;
        problems.extend(scan(&relative, &text, Origin::HandWritten, &names));
    }
    for artifact in artifacts {
        let page = Path::new(artifact.path);
        if page.extension().is_none_or(|e| e != "md") {
            continue;
        }
        problems.extend(scan(page, &artifact.contents, Origin::Generated, &names));
    }
    if problems.is_empty() {
        return Ok(());
    }
    for problem in &problems {
        eprintln!("{problem}");
    }
    Err(format!(
        "{} tool reference(s) do not match the registry; name tools as links to entries of {TOOLS_REFERENCE}",
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
pub(crate) fn scan(page: &Path, text: &str, origin: Origin, names: &Names) -> Vec<String> {
    if text.contains(OPT_OUT) {
        return Vec::new();
    }
    let mut problems = Vec::new();
    let mut fenced = false;
    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let mut found = Vec::new();
        let links = scan_links(page, line, names, &mut found);
        scan_code(line, &links, origin, names, &mut found);
        let at = format!("{}:{}", page.display(), number + 1);
        problems.extend(found.into_iter().map(|p| format!("{at}: {p}")));
    }
    problems
}

/// Checks the links of `line` to the tools reference; returns the byte
/// ranges of every link so their labels are not read as bare names.
fn scan_links(
    page: &Path,
    line: &str,
    names: &Names,
    found: &mut Vec<String>,
) -> Vec<Range<usize>> {
    let dir = page.parent().unwrap_or(Path::new(""));
    let mut ranges = Vec::new();
    for link in LINK.captures_iter(line) {
        ranges.push(link.get(0).expect("whole match").range());
        let target = if link[2].is_empty() {
            page.to_path_buf()
        } else {
            normalize(&dir.join(&link[2]))
        };
        if target != Path::new(TOOLS_REFERENCE) {
            continue;
        }
        let (label, anchor) = (link[1].trim_matches('`'), &link[3]);
        if !names.tools.contains(anchor) {
            found.push(format!("unknown tool '{anchor}'"));
        } else if label != anchor {
            found.push(format!("link text '{label}' names tool '{anchor}'"));
        }
    }
    ranges
}

/// Checks the inline-code names of `line` outside `links`.
fn scan_code(
    line: &str,
    links: &[Range<usize>],
    origin: Origin,
    names: &Names,
    found: &mut Vec<String>,
) {
    for code in CODE.captures_iter(line) {
        let span = code.get(0).expect("whole match");
        if links.iter().any(|r| r.contains(&span.start())) {
            continue;
        }
        let context =
            TOOL_AFTER.is_match(&line[span.end()..]) || TOOL_BEFORE.is_match(&line[..span.start()]);
        found.extend(check_name(&code[1], context, origin, names));
    }
}

/// The problem with an inline-code `name`, if any.
fn check_name(name: &str, context: bool, origin: Origin, names: &Names) -> Option<String> {
    if !IDENT.is_match(name) {
        return None;
    }
    let shared = names.shared.contains(name);
    if names.tools.contains(name) {
        let must_link = origin == Origin::HandWritten && (context || !shared);
        return must_link
            .then(|| format!("tool `{name}` must link to its reference entry (tools.md#{name})"));
    }
    if shared {
        // A toolset or key: "every `core` tool" names the toolset's tools.
        return None;
    }
    if let Some(tool) = names.extended(name) {
        return Some(format!("unknown tool '{name}' (did you mean `{tool}`?)"));
    }
    context.then(|| format!("unknown tool '{name}'"))
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
    use std::path::Path;

    use super::{Names, OPT_OUT, Origin, scan};

    const GUIDE: &str = "docs/guide/agent.md";

    fn guide(text: &str) -> Vec<String> {
        scan(
            Path::new(GUIDE),
            text,
            Origin::HandWritten,
            &Names::builtin(),
        )
    }

    fn generated(text: &str) -> Vec<String> {
        let page = Path::new("docs/reference/cli.md");
        scan(page, text, Origin::Generated, &Names::builtin())
    }

    #[test]
    fn renamed_tool_fails_the_check() {
        assert!(guide("Track steps with [`todo`](../reference/tools.md#todo).").is_empty());
        assert_eq!(
            guide("Track steps with [`todo_list`](../reference/tools.md#todo_list)."),
            ["docs/guide/agent.md:1: unknown tool 'todo_list'"]
        );
        assert_eq!(
            guide("[`todo_list`](../reference/tools.md#todo)"),
            ["docs/guide/agent.md:1: link text 'todo_list' names tool 'todo'"]
        );
        // Unlinked: in a plain sentence, in a tool context and in generated
        // help text alike.
        let expected = "unknown tool 'todo_list' (did you mean `todo`?)";
        for text in ["Call `todo_list` to plan.", "The `todo_list` tool plans."] {
            assert_eq!(guide(text), [format!("{GUIDE}:1: {expected}")], "{text}");
        }
        assert_eq!(
            generated("Plans with the `todo_list` tool."),
            [format!("docs/reference/cli.md:1: {expected}")]
        );
    }

    #[test]
    fn an_unknown_name_in_a_tool_context_fails() {
        let expected = ["docs/guide/agent.md:1: unknown tool 'scratchpad'"];
        assert_eq!(guide("The `scratchpad` tool keeps notes."), expected);
        assert_eq!(guide("Use tool `scratchpad`."), expected);
        // Not a tool context: a toolset, a config key, a word near "tools".
        assert!(guide("The `scratchpad` toolset and `tools.enabled_toolsets`.").is_empty());
        assert!(guide("`logs` and `tools` are reserved.").is_empty());
        assert!(guide("Every `core` tool except one.").is_empty());
    }

    #[test]
    fn hand_written_pages_link_registered_tools() {
        assert_eq!(
            guide("Edit files with `patch`."),
            [
                "docs/guide/agent.md:1: tool `patch` must link to its reference entry (tools.md#patch)"
            ]
        );
        // `terminal` and `process` are also toolsets: tools only in a tool
        // context.
        assert!(guide("`debugging` is `terminal` and `process`.").is_empty());
        assert_eq!(guide("The `process` tool polls.").len(), 1);
        // Generated pages cannot link.
        assert!(generated("How long the `clarify` tool waits; see `patch`.").is_empty());
    }

    #[test]
    fn code_blocks_other_links_and_opted_out_pages_are_skipped() {
        assert!(guide("```toml\nx = \"`todo_list`\"\n```\n").is_empty());
        assert!(guide("[x](tools.md#choosing-toolsets)").is_empty());
        assert_eq!(generated("[`nope`](tools.md#nope)").len(), 1);
        assert!(
            guide(&format!(
                "{OPT_OUT} -- Hermes -->\nHermes calls it `todo_list`."
            ))
            .is_empty()
        );
    }

    #[test]
    fn the_repository_docs_pass() {
        let result = super::check(&crate::repo_root(), &crate::artifacts::all());
        assert!(result.is_ok(), "{result:?}");
    }
}
