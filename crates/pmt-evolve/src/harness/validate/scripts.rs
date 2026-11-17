//! Scripts: parsed, never run; and every script the harness text names
//! must be in the tree.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use pmt_config::harness::Tree;
use pmt_tools::files::lint::{LintStatus, Linter};
use regex::Regex;

use crate::harness::surface;

/// Parses every `.py` (Python grammar) and `.sh` (the shell's own `-n`
/// syntax check) script.
pub(super) fn syntax(tree: &Tree, errors: &mut Vec<String>) {
    let linter = Linter::builtin();
    for (path, text) in tree.files.iter().filter(|(p, _)| surface::is_script(p)) {
        let verdict = if Path::new(path).extension().is_some_and(|e| e == "sh") {
            shell(text)
        } else {
            python(&linter, path, text)
        };
        if let Err(message) = verdict {
            errors.push(format!("{path}: {message}"));
        }
    }
}

/// `bash -n` (else `sh -n`) over the script on stdin: the shell reads and
/// parses every command and runs none. The shell is used instead of an
/// embedded grammar because a bash grammar costs about 1.4 MiB of binary
/// against the 80 MiB budget (spec: workspace-architecture, Memory and size
/// budgets), and a harness `.sh` hook needs that shell to run anyway.
fn shell(text: &str) -> Result<(), String> {
    let spawned = ["bash", "sh"].into_iter().find_map(|name| {
        let mut command = Command::new(name);
        if name == "bash" {
            command.args(["--norc", "--noprofile"]);
        }
        command
            .arg("-n")
            // A non-interactive shell sources these before reading stdin.
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .ok()
            .map(|child| (name, child))
    });
    let Some((name, mut child)) = spawned else {
        return Err("could not be checked (no bash or sh on PATH)".to_owned());
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A shell that stops at the first error closes stdin early; its
        // verdict is in the exit status, not in this write.
        let _ = stdin.write_all(text.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("could not be checked ({name}: {e})"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let first = stderr.lines().next().unwrap_or("syntax error").trim();
    // `bash: line 1: syntax error ...` / `sh: 1: Syntax error: ...`.
    Err(first
        .strip_prefix(&format!("{name}: "))
        .unwrap_or(first)
        .to_owned())
}

fn python(linter: &Linter, path: &str, text: &str) -> Result<(), String> {
    let lint = linter.check(Path::new(path), text);
    match lint.status {
        LintStatus::Ok => Ok(()),
        LintStatus::Error => Err(lint.output),
        LintStatus::Skipped => Err(format!("could not be checked ({})", lint.output)),
    }
}

/// A `scripts/<file>` or `skills/<name>/scripts/<file>` reference: not
/// preceded by a path character, so `x/scripts/a.py` is not one.
static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[^A-Za-z0-9_./-])((?:skills/[a-z0-9][a-z0-9_-]{0,63}/)?scripts/[A-Za-z0-9_][A-Za-z0-9_.-]*\.(?:py|sh))\b",
    )
    .expect("script reference pattern")
});

/// Every script named in the harness's Markdown exists in the tree.
pub(super) fn references(tree: &Tree, errors: &mut Vec<String>) {
    for (path, text) in tree
        .files
        .iter()
        .filter(|(p, _)| Path::new(p).extension().is_some_and(|e| e == "md"))
    {
        let mut missing: Vec<&str> = REFERENCE
            .captures_iter(text)
            .filter_map(|c| c.get(1))
            .map(|m| m.as_str())
            .filter(|named| !tree.files.contains_key(*named))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        for named in missing {
            errors.push(format!("{path} names {named}, which is not in the harness"));
        }
    }
}

/// The harness scripts `words` of a hook command name.
pub(super) fn named_in(words: &[String]) -> impl Iterator<Item = &str> {
    words
        .iter()
        .map(|w| w.trim_start_matches("./"))
        .filter(|w| w.contains('/') || pmt_config::harness::is_script(w))
}
