//! Size caps (spec: harness-evolution, Harness size caps).

use pmt_config::harness::Tree;

/// Largest single file, in bytes.
pub const MAX_FILE_BYTES: usize = 65_536;
/// Largest whole tree, in bytes.
pub const MAX_TREE_BYTES: usize = 1_048_576;
/// Most files.
pub const MAX_FILES: usize = 200;
/// Most skills.
pub const MAX_SKILLS: usize = 32;
/// Most subagent definitions.
pub const MAX_AGENTS: usize = 8;

/// The per-file, whole-tree and count caps.
pub(super) fn files(tree: &Tree, errors: &mut Vec<String>) {
    let mut total = 0;
    for (path, text) in &tree.files {
        total += text.len();
        if text.len() > MAX_FILE_BYTES {
            errors.push(format!(
                "{path} is {} bytes; cap is {MAX_FILE_BYTES}",
                text.len()
            ));
        }
    }
    if total > MAX_TREE_BYTES {
        errors.push(format!("harness is {total} bytes; cap is {MAX_TREE_BYTES}"));
    }
    let count = |prefix: &str, suffix: &str| {
        tree.files
            .keys()
            .filter(|p| p.starts_with(prefix) && p.ends_with(suffix))
            .count()
    };
    let limits = [
        ("files", tree.files.len(), MAX_FILES),
        ("skills", count("skills/", "/SKILL.md"), MAX_SKILLS),
        (
            "subagent definitions",
            count("agents/", "/AGENT.md"),
            MAX_AGENTS,
        ),
    ];
    for (what, n, cap) in limits {
        if n > cap {
            errors.push(format!("harness has {n} {what}; cap is {cap}"));
        }
    }
}

/// The always-on cap, checked after trigger repair.
pub(super) fn always_on(tree: &Tree, cap: usize, errors: &mut Vec<String>) {
    let chars = tree.always_on_chars();
    if chars > cap {
        errors.push(format!("always-on text is {chars} chars; cap is {cap}"));
    }
}
