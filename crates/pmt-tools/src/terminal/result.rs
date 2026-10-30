//! The `terminal` result (spec: execution-environments, Foreground result
//! shaping; Infrastructure failures are degraded results).

use serde::Serialize;

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Exit status 0.
    Ok,
    /// A non-zero exit, or the call was refused before running.
    Error,
    /// The foreground timeout stopped the command.
    Timeout,
    /// The execution environment failed; the next call recreates it.
    Degraded,
    /// A background process started.
    Started,
}

/// One `terminal` result. Optional fields are left out when unset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TerminalResult {
    /// Shaped output (ANSI stripped, redacted, truncated).
    pub output: String,
    /// The exit code; `-1` when nothing ran or the environment failed,
    /// `None` for a background start.
    pub exit_code: Option<i32>,
    /// How the call ended.
    pub status: Status,
    /// Why nothing ran (argument, guard or sandbox refusals).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The directory the command finished in, when it differs from the one
    /// it started in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// What a non-zero exit code usually means.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code_meaning: Option<String>,
    /// Advice for the next step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The environment was rebuilt after a failure.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub environment_recreated: bool,
    /// The output was cut to `tool_output.max_chars`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// The full redacted output (0600).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    /// The background process id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_id: Option<String>,
    /// A foreground call over `terminal.max_foreground_secs` became a
    /// background process.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub promoted_from_foreground: bool,
    /// Why the environment is degraded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What to do about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_hint: Option<String>,
    /// One-time notices (ignored paths, shell fallbacks, skipped init files).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<String>,
}

impl TerminalResult {
    /// An empty result with `status`.
    #[must_use]
    pub fn new(status: Status) -> Self {
        Self {
            output: String::new(),
            exit_code: Some(-1),
            status,
            error: None,
            cwd: None,
            exit_code_meaning: None,
            hint: None,
            environment_recreated: false,
            truncated: false,
            full_output_path: None,
            process_id: None,
            promoted_from_foreground: false,
            reason: None,
            retry_hint: None,
            notices: Vec::new(),
        }
    }

    /// A refusal: nothing ran.
    #[must_use]
    pub fn refused(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::new(Status::Error)
        }
    }

    /// The result as JSON.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }
}
