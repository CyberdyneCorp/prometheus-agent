//! `/init [notes]` (spec: agent-turn-loop, Project instructions bootstrap
//! with /init).
//!
//! `/init` runs a normal turn (origin `init`) told to inspect the
//! workspace with read-only tools and then write `AGENTS.md` at the
//! workspace root, or merge-update the existing one; the write goes
//! through the normal file tools and their approvals. The host keeps the
//! user's sections: every `## ` section the file had before the turn that
//! is no longer present verbatim afterwards is appended back. The file
//! becomes project context from the next session (the running session's
//! prompt is frozen).

use std::path::{Path, PathBuf};

use crate::prompt::context::git_root;
use crate::turn::TurnInput;

/// The origin of an `/init` turn.
pub const ORIGIN: &str = "init";

/// The instruction an `/init` turn starts from.
pub const INSTRUCTION: &str = "Create or update the project instructions file AGENTS.md at the \
workspace root. First inspect the workspace with read-only tools: the manifests and build \
files, the directory layout, the toolchain and the CI configuration. Then write AGENTS.md \
with what an agent needs to work here: how to build, test, lint and run the project, the \
layout, and the conventions you found. If AGENTS.md already exists, merge your findings \
into it and keep every section the user wrote verbatim.";

/// The `/init` turn's input; `notes` are added as emphasis.
#[must_use]
pub fn input(notes: &str) -> TurnInput {
    let notes = notes.trim();
    let text = if notes.is_empty() {
        INSTRUCTION.to_owned()
    } else {
        format!("{INSTRUCTION}\n\nPay particular attention to: {notes}")
    };
    TurnInput {
        origin: Some(ORIGIN.to_owned()),
        ..TurnInput::text(text)
    }
}

/// Whether `input` is an `/init` turn: its origin, or its text when a
/// client submitted the command's message (daemon and gateway surfaces
/// send the text only).
#[must_use]
pub fn is_init(input: &TurnInput) -> bool {
    input.origin.as_deref() == Some(ORIGIN) || input.text.starts_with(INSTRUCTION)
}

/// `AGENTS.md` at the workspace root of `cwd`.
#[must_use]
pub fn agents_file(cwd: &Path) -> PathBuf {
    git_root(cwd)
        .unwrap_or_else(|| cwd.to_path_buf())
        .join("AGENTS.md")
}

/// The `## ` sections of `text`, each with its heading and body.
#[must_use]
pub fn sections(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let heading = line.starts_with("## ") || (line.starts_with("# ") && current.is_some());
        if heading {
            if let Some(done) = current.take() {
                found.push(done.trim_end().to_owned());
            }
            if line.starts_with("## ") {
                current = Some(String::new());
            }
        }
        if let Some(section) = &mut current {
            section.push_str(line);
            section.push('\n');
        }
    }
    if let Some(done) = current {
        found.push(done.trim_end().to_owned());
    }
    found
}

/// `after` with every section of `before` it lost appended back; `None`
/// when nothing was lost.
#[must_use]
pub fn restore(before: &str, after: &str) -> Option<String> {
    let lost: Vec<String> = sections(before)
        .into_iter()
        .filter(|s| !after.contains(s.as_str()))
        .collect();
    if lost.is_empty() {
        return None;
    }
    let mut merged = after.trim_end().to_owned();
    for section in lost {
        merged.push_str("\n\n");
        merged.push_str(&section);
    }
    merged.push('\n');
    Some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = "# Project\n\nIntro.\n\n## Release rules\n\nTag from main only.\nNever force-push.\n\n## Old notes\n\nx\n";

    #[test]
    fn user_sections_are_found_with_their_bodies() {
        assert_eq!(
            sections(BEFORE),
            [
                "## Release rules\n\nTag from main only.\nNever force-push.",
                "## Old notes\n\nx"
            ]
        );
    }

    #[test]
    fn a_lost_section_is_appended_back_verbatim() {
        let after = "# Project\n\n## Build\n\ncargo build\n\n## Old notes\n\nx\n";
        let merged = restore(BEFORE, after).unwrap();
        assert!(merged.contains("## Release rules\n\nTag from main only.\nNever force-push."));
        assert_eq!(merged.matches("## Old notes").count(), 1);
        assert_eq!(restore(BEFORE, BEFORE), None);
    }

    #[test]
    fn notes_are_emphasized() {
        let input = input("focus on the test commands");
        assert_eq!(input.origin.as_deref(), Some(ORIGIN));
        assert!(
            input
                .text
                .ends_with("Pay particular attention to: focus on the test commands")
        );
    }
}
