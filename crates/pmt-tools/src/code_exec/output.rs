//! What a script run returns (spec: subagent-delegation, `execute_code`
//! RPC channel and limits): stdout capped at [`STDOUT_CAP`] bytes (its
//! first 40% and last 60%, the full text kept in a file the session can
//! read), stderr capped at [`STDERR_CAP`], the exit code, the number of
//! tool calls and whether the time limit stopped it.

use std::path::Path;
use std::time::Duration;

use serde_json::json;

use crate::ToolResult;

/// Most stdout bytes returned.
pub const STDOUT_CAP: usize = 50_000;
/// Most stderr bytes returned.
pub const STDERR_CAP: usize = 10_000;

/// One finished run.
#[allow(clippy::struct_excessive_bools)] // independent outcomes of one run
pub struct Ran<'a> {
    /// Captured stdout.
    pub stdout: &'a [u8],
    /// Captured stderr.
    pub stderr: &'a [u8],
    /// The exit code (`None` when killed).
    pub exit_code: Option<i32>,
    /// The time limit stopped it.
    pub timed_out: bool,
    /// The turn's stop killed it.
    pub cancelled: bool,
    /// Stub calls dispatched.
    pub tool_calls: u32,
    /// The time limit.
    pub timeout: Duration,
    /// Where the full stdout is kept when capped.
    pub spill_dir: &'a Path,
}

/// `text` within `cap` bytes: its first 40% and last 60% around a marker
/// (on character boundaries); `None` when it fits.
#[must_use]
pub fn cap_stdout(text: &str, cap: usize) -> Option<String> {
    if text.len() <= cap {
        return None;
    }
    let marker = format!("\n[... {} bytes omitted ...]\n", text.len() - cap);
    let room = cap.saturating_sub(marker.len());
    let mut head = room * 2 / 5;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (room - head);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    Some(format!("{}{marker}{}", &text[..head], &text[tail..]))
}

/// The tool result of `ran`.
#[must_use]
pub fn result(ran: &Ran<'_>) -> ToolResult {
    finish(&body(ran), ran.exit_code == Some(0))
}

/// The JSON body of `ran`'s result (callers may add fields before
/// [`finish`]).
#[must_use]
pub fn body(ran: &Ran<'_>) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(ran.stdout);
    let stderr = String::from_utf8_lossy(ran.stderr);
    let mut full_output = None;
    let shown = match cap_stdout(&stdout, STDOUT_CAP) {
        Some(capped) => {
            full_output = keep(ran.spill_dir, &stdout);
            capped
        }
        None => stdout.into_owned(),
    };
    let stderr = cap_stdout(&stderr, STDERR_CAP).unwrap_or_else(|| stderr.into_owned());
    let mut body = json!({
        "stdout": shown,
        "stderr": stderr,
        "exit_code": ran.exit_code,
        "tool_calls": ran.tool_calls,
    });
    if let Some(path) = full_output {
        body["full_output"] = path.into();
    }
    if ran.timed_out {
        body["error"] = format!(
            "The script ran longer than {} s (code_execution.timeout_secs) and was stopped.",
            ran.timeout.as_secs()
        )
        .into();
    }
    if ran.cancelled {
        body["error"] = "The script was stopped with the turn.".into();
    }
    body
}

/// The tool result of a finished `body`: an error unless the script
/// exited with 0.
#[must_use]
pub fn finish(body: &serde_json::Value, ok: bool) -> ToolResult {
    let text = body.to_string();
    if ok {
        ToolResult::ok(text)
    } else {
        ToolResult::error(text)
    }
}

fn keep(dir: &Path, stdout: &str) -> Option<String> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("execute-code-{}.txt", ulid::Ulid::generate()));
    std::fs::write(&path, stdout).ok()?;
    Some(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_output_keeps_its_head_and_tail() {
        let text: String = (0..80_000)
            .map(|i| if i < 40_000 { 'a' } else { 'z' })
            .collect();
        let capped = cap_stdout(&text, STDOUT_CAP).unwrap();
        assert!(capped.len() <= STDOUT_CAP);
        assert!(capped.starts_with(&"a".repeat(19_000)));
        assert!(capped.ends_with(&"z".repeat(29_000)));
        assert!(cap_stdout("short", STDOUT_CAP).is_none());
    }
}
