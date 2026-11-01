//! Unified diffs of file tool edits.

use similar::TextDiff;

/// A unified diff (3 lines of context) from `old` to `new` of `path`;
/// `None` side means the file did not exist (added) or no longer exists
/// (deleted).
pub(crate) fn unified(path: &str, old: Option<&str>, new: Option<&str>) -> String {
    let before = old.unwrap_or_default();
    let after = new.unwrap_or_default();
    let from = if old.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".to_owned()
    };
    let to = if new.is_some() {
        format!("b/{path}")
    } else {
        "/dev/null".to_owned()
    };
    TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header(&from, &to)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::unified;

    #[test]
    fn diff_names_both_sides() {
        let diff = unified("a.rs", Some("x\ny\n"), Some("x\nz\n"));
        assert!(diff.starts_with("--- a/a.rs\n+++ b/a.rs\n"), "{diff}");
        assert!(diff.contains("-y\n+z\n"), "{diff}");
        assert!(unified("n.rs", None, Some("a\n")).starts_with("--- /dev/null\n+++ b/n.rs"));
    }
}
