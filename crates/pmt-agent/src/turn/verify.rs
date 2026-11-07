//! Verify-on-stop (spec: agent-turn-loop, Verify-on-stop).
//!
//! With `agent.verify_on_stop` on (`true`, or `auto` on CLI, TUI, desktop,
//! ACP and API callers), a turn that edited non-documentation files must
//! show fresh verification evidence (a test, build, lint or type-check
//! command run after the last edit) before its final answer stands;
//! otherwise a cache-safe nudge (a new message) asks the model to verify
//! or explain why it cannot (its next answer stands unless it edits
//! again), at most `agent.max_verify_nudges` times per
//! turn together with `pre_verify` hook nudges. The evidence ledger goes
//! to `state.db` only while the feature is on.

use std::collections::BTreeSet;
use std::path::Path;

use pmt_config::schema::{AutoMode, VerifyOnStop};
use serde_json::Value;

use super::model::RequestedCall;
use super::observe::edited_paths;
use super::run::Turn;

/// Surfaces `auto` turns the feature on for.
const AUTO_SURFACES: &[&str] = &["cli", "tui", "desktop", "acp", "api"];

/// Appended to the nudge with `agent.verify_guidance`.
pub(super) const GUIDANCE: &str = "Before you finish, review the diff: keep it minimal and focused on the request, and remove debugging output and unrelated edits.";

/// Whether the feature is on for `platform`.
#[must_use]
pub(super) fn enabled(setting: VerifyOnStop, platform: &str) -> bool {
    match setting {
        VerifyOnStop::Enabled(on) => on,
        VerifyOnStop::Mode(AutoMode::Auto) => AUTO_SURFACES.contains(&platform),
    }
}

/// Whether `path` is documentation (its edits need no verification).
#[must_use]
pub(super) fn is_documentation(path: &str) -> bool {
    let p = Path::new(path);
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if matches!(
        ext.as_deref(),
        Some("md" | "markdown" | "mdx" | "rst" | "txt" | "adoc" | "org")
    ) {
        return true;
    }
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_uppercase();
    ["README", "CHANGELOG", "LICENSE", "AUTHORS", "NOTICE"]
        .iter()
        .any(|doc| name.starts_with(doc))
        || p.components().any(|c| c.as_os_str() == "docs")
}

/// Programs that verify on their own, and programs that verify with one
/// of the listed subcommands.
const CHECKERS: &[&str] = &[
    "pytest",
    "tsc",
    "eslint",
    "ruff",
    "mypy",
    "pyright",
    "jest",
    "vitest",
    "tox",
    "nox",
    "rspec",
    "phpunit",
    "ctest",
    "clippy-driver",
    "shellcheck",
    "golangci-lint",
];
const RUNNERS: &[(&str, &[&str])] = &[
    (
        "cargo",
        &["test", "build", "check", "clippy", "nextest", "fmt"],
    ),
    ("npm", &["test", "run", "t"]),
    ("pnpm", &["test", "run", "lint", "build", "typecheck"]),
    ("yarn", &["test", "run", "lint", "build", "typecheck"]),
    ("bun", &["test", "run"]),
    ("go", &["test", "build", "vet"]),
    ("make", &["test", "check", "build", "lint", "all"]),
    ("just", &["test", "check", "build", "lint"]),
    ("gradle", &["test", "build", "check"]),
    ("./gradlew", &["test", "build", "check"]),
    ("mvn", &["test", "verify", "package"]),
    ("dotnet", &["test", "build"]),
    ("deno", &["test", "check", "lint"]),
    ("swift", &["test", "build"]),
    ("python", &["-m"]),
    ("python3", &["-m"]),
    ("uv", &["run"]),
];

/// Whether a shell command runs a test, build, lint or type check.
#[must_use]
pub(super) fn is_check(command: &str) -> bool {
    command
        .split(['&', ';', '|', '\n'])
        .any(|part| check_words(&part.split_whitespace().collect::<Vec<_>>()))
}

fn check_words(words: &[&str]) -> bool {
    let words: Vec<&str> = words
        .iter()
        .copied()
        .skip_while(|w| w.contains('=') || matches!(*w, "sudo" | "time" | "env"))
        .collect();
    let Some(program) = words.first() else {
        return false;
    };
    let name = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program);
    if CHECKERS.contains(&name) || program.ends_with("scripts/check") {
        return true;
    }
    RUNNERS.iter().any(|(runner, subs)| {
        (*runner == *program || *runner == name)
            && (subs.is_empty() || words.get(1).is_some_and(|s| subs.contains(s)))
    })
}

/// The command a `terminal` call runs.
fn command_of(call: &RequestedCall) -> Option<String> {
    if call.name != "terminal" {
        return None;
    }
    let args: Value = serde_json::from_str(&call.arguments).ok()?;
    args.get("command")?.as_str().map(str::to_owned)
}

/// What the turn edited without verifying since.
#[derive(Debug, Default)]
pub(super) struct Evidence {
    unverified: BTreeSet<String>,
}

impl Turn<'_> {
    fn verify_on(&self) -> bool {
        enabled(
            self.config.agent.verify_on_stop,
            &self.agent.setup.prompt.platform,
        )
    }

    /// Takes the evidence of one finished call.
    pub(super) fn observe_evidence(&mut self, call: &RequestedCall) {
        if !self.verify_on() {
            return;
        }
        let edits: Vec<String> = edited_paths(&call.name, &call.arguments)
            .into_iter()
            .filter(|p| !is_documentation(p))
            .collect();
        let check = command_of(call).filter(|c| is_check(c));
        for path in &edits {
            self.record_evidence("edit", path);
            self.evidence.unverified.insert(path.clone());
        }
        if let Some(command) = check {
            self.record_evidence("check", &command);
            self.evidence.unverified.clear();
        }
    }

    fn record_evidence(&self, kind: &str, detail: &str) {
        let setup = &self.agent.setup;
        if let Err(err) =
            setup
                .store
                .record_verify_evidence(setup.session, self.id.as_str(), kind, detail)
        {
            tracing::warn!(error = %err, "verify-on-stop evidence not recorded");
        }
    }

    /// The nudge before a final answer that left edits unverified.
    pub(super) fn verify_on_stop(&mut self) -> Option<String> {
        if !self.verify_on()
            || self.evidence.unverified.is_empty()
            || self.verify_nudges >= self.config.agent.max_verify_nudges
        {
            return None;
        }
        self.verify_nudges += 1;
        // The answer to this nudge (verified, or why it cannot be) stands
        // unless the model edits again.
        let unverified = std::mem::take(&mut self.evidence.unverified);
        let files: Vec<&str> = unverified.iter().map(String::as_str).collect();
        let mut text = format!(
            "[prometheus] You changed {} in this turn but ran no test, build, lint or type-check command after the last edit. Verify the change now, or explain why it cannot be verified.",
            files.join(", ")
        );
        if self.config.agent.verify_guidance {
            text.push(' ');
            text.push_str(GUIDANCE);
        }
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documentation_is_recognized_by_extension_name_and_directory() {
        for doc in [
            "README.md",
            "notes.txt",
            "docs/guide/x.rs",
            "CHANGELOG",
            "a/b.rst",
        ] {
            assert!(is_documentation(doc), "{doc}");
        }
        for code in ["src/lib.rs", "Cargo.toml", "web/app.ts"] {
            assert!(!is_documentation(code), "{code}");
        }
    }

    #[test]
    fn test_build_lint_and_type_checks_are_evidence() {
        for check in [
            "cargo test -p foo",
            "cd crates/x && cargo clippy --all-targets",
            "RUST_LOG=debug cargo nextest run",
            "npm test",
            "python -m pytest -q",
            "pytest tests/",
            "./scripts/check --no-perf",
            "go vet ./...",
            "tsc --noEmit | tail",
        ] {
            assert!(is_check(check), "{check}");
        }
        for other in ["ls -la", "git diff", "cat src/lib.rs", "echo cargo test"] {
            assert!(!is_check(other), "{other}");
        }
    }

    #[test]
    fn auto_is_on_for_interactive_and_api_callers_only() {
        let auto = VerifyOnStop::Mode(AutoMode::Auto);
        assert!(enabled(auto, "cli") && enabled(auto, "api"));
        assert!(!enabled(auto, "telegram"));
        assert!(!enabled(VerifyOnStop::Enabled(false), "cli"));
        assert!(enabled(VerifyOnStop::Enabled(true), "slack"));
    }
}
