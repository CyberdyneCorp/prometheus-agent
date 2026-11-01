//! Display preferences that shape the line interface's layout (spec: cli,
//! Display preferences): the final answer's Markdown
//! (`display.final_response_markdown`), inline diffs of file edits
//! (`display.inline_diffs`) and the compact layout (`display.compact`).
//! `display.vim_mode` belongs to the editor ([`super::input`]).

use pmt_config::schema::{MarkdownMode, ToolProgress};

use super::Repl;
use crate::chat::markdown;
use crate::chat::theme::Role;

/// The file tool whose results carry a unified diff.
const EDIT_TOOL: &str = "patch";

impl Repl {
    /// Prints the turn's final answer, rendered or raw.
    pub(super) fn final_answer(&self, text: &str) {
        let text = match self.factory.scope.config().display.final_response_markdown {
            MarkdownMode::Render => markdown::render(text, self.display.color),
            MarkdownMode::Raw => text.to_owned(),
        };
        self.say(Role::Assistant, &self.stamped(&text));
    }

    /// Prints the diff of a successful file edit under its tool line, when
    /// inline diffs are on and tool activity is shown.
    pub(super) fn inline_diff(&mut self, tool: &str, output: &str) {
        let shown = self.display.focus.is_none() && self.display.progress != ToolProgress::Off;
        if tool != EDIT_TOOL || !shown || !self.factory.scope.config().display.inline_diffs {
            return;
        }
        let Some(diff) = edit_diff(output) else {
            return;
        };
        // The accumulated tool line belongs above its diff.
        self.flush_group();
        for line in diff.lines() {
            self.say(diff_role(line), &format!("    {line}"));
        }
    }

    /// The blank line that separates the banner and each turn, unless the
    /// layout is compact (or quiet).
    pub(super) fn spacer(&self) {
        if !self.factory.scope.config().display.compact && !self.display.quiet {
            self.input.print("");
        }
    }
}

/// The unified diff in a `patch` result, if it has one.
fn edit_diff(output: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    let diff = value.get("diff")?.as_str()?.trim_end();
    (!diff.is_empty()).then(|| diff.to_owned())
}

/// The colour of one diff line.
fn diff_role(line: &str) -> Role {
    if line.starts_with("+++") || line.starts_with("---") {
        Role::Tool
    } else if line.starts_with('+') {
        Role::Added
    } else if line.starts_with('-') {
        Role::Removed
    } else if line.starts_with("@@") {
        Role::Accent
    } else {
        Role::Tool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_results_with_a_diff_have_one() {
        let ok = r#"{"path":"a.txt","diff":"--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-x\n+y\n"}"#;
        assert_eq!(
            edit_diff(ok).as_deref(),
            Some("--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-x\n+y")
        );
        assert_eq!(edit_diff(r#"{"diff":""}"#), None);
        assert_eq!(edit_diff("not json"), None);
    }

    #[test]
    fn diff_lines_are_coloured_by_kind() {
        assert_eq!(diff_role("+++ b/a"), Role::Tool);
        assert_eq!(diff_role("+y"), Role::Added);
        assert_eq!(diff_role("-x"), Role::Removed);
        assert_eq!(diff_role("@@ -1 +1 @@"), Role::Accent);
        assert_eq!(diff_role(" same"), Role::Tool);
    }
}
