//! The coding posture (spec: agent-turn-loop, Coding posture and standing
//! coding instructions).
//!
//! Per `agent.coding_context` (`auto`: interactive CLI, TUI, desktop and
//! ACP sessions whose working directory is a code workspace; `on`; `off`)
//! the session tier gets a coding brief, a workspace snapshot (branch and
//! dirty-file count) and `agent.coding_instructions`. It is built once,
//! with the session's prompt, so a change applies from the next session.

use std::path::Path;
use std::process::Command;

use pmt_config::schema::{AgentConfig, CodingContext};

use super::context::git_root;

/// The brief every coding session starts with.
pub const BRIEF: &str = "# Coding\n\
You are working in a code workspace. Read the relevant code before changing it, \
keep changes small and focused on the request, follow the project's conventions, \
and verify your edits by running the project's tests, build, linter or type checker \
before you report the work as done.";

/// Files that mark a directory as a code workspace (besides a git root).
const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "CMakeLists.txt",
    "Makefile",
    "Gemfile",
    "composer.json",
    "mix.exs",
    "deno.json",
];

/// Whether `cwd` is a code workspace: inside a git repository or holding a
/// build manifest.
#[must_use]
pub fn is_workspace(cwd: &Path) -> bool {
    git_root(cwd).is_some() || MANIFESTS.iter().any(|m| cwd.join(m).is_file())
}

/// Whether the posture applies to a session on `platform` in `cwd`.
#[must_use]
pub fn applies(agent: &AgentConfig, platform: &str, cwd: Option<&Path>) -> bool {
    match agent.coding_context {
        CodingContext::Off => false,
        CodingContext::On => true,
        CodingContext::Auto => {
            crate::turn::interactive_platform(platform) && cwd.is_some_and(is_workspace)
        }
    }
}

/// The brief, the workspace snapshot and the standing instructions.
#[must_use]
pub fn block(agent: &AgentConfig, cwd: Option<&Path>) -> String {
    let mut text = BRIEF.to_owned();
    if let Some(snapshot) = cwd.and_then(snapshot) {
        text.push_str("\n\n");
        text.push_str(&snapshot);
    }
    let standing = agent.coding_instructions.trim();
    if !standing.is_empty() {
        text.push_str("\n\nStanding coding instructions:\n");
        text.push_str(standing);
    }
    text
}

/// `Workspace: branch <b>, <n> dirty files` for a git workspace.
fn snapshot(cwd: &Path) -> Option<String> {
    let root = git_root(cwd)?;
    let branch = branch(&root).unwrap_or_else(|| "(detached)".to_owned());
    let dirty = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain=v1"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count());
    Some(match dirty {
        Some(n) => format!("Workspace: branch {branch}, {n} dirty files"),
        None => format!("Workspace: branch {branch}"),
    })
}

/// The checked-out branch, read from `.git/HEAD` (no process).
fn branch(root: &Path) -> Option<String> {
    let git = root.join(".git");
    let head_file = if git.is_file() {
        // A worktree or submodule: `gitdir: <path>`.
        let pointer = std::fs::read_to_string(&git).ok()?;
        let dir = pointer.strip_prefix("gitdir:")?.trim();
        root.join(dir).join("HEAD")
    } else {
        git.join("HEAD")
    };
    let head = std::fs::read_to_string(head_file).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_is_on_for_interactive_code_workspaces_only() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        std::fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        let plain = tempfile::tempdir().unwrap();
        let agent = AgentConfig::default();
        assert!(applies(&agent, "cli", Some(repo.path())));
        assert!(!applies(&agent, "telegram", Some(repo.path())));
        assert!(!applies(&agent, "cli", Some(plain.path())));
        let on = AgentConfig {
            coding_context: CodingContext::On,
            ..AgentConfig::default()
        };
        assert!(applies(&on, "telegram", None));
        assert_eq!(branch(repo.path()).as_deref(), Some("feature/x"));
    }

    #[test]
    fn standing_instructions_follow_the_brief() {
        let agent = AgentConfig {
            coding_instructions: "Always run just check.".into(),
            ..AgentConfig::default()
        };
        let text = block(&agent, None);
        assert!(text.starts_with(BRIEF));
        assert!(text.ends_with("Standing coding instructions:\nAlways run just check."));
    }
}
