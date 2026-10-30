//! Command approvals (spec: tool-system, Approval modes are typed; Approval
//! floors cannot be bypassed; Human approval choices and grants file;
//! Unattended contexts never wait; Approval channel fails closed;
//! Content-level dangerous-command detectors; Approval dry-run test
//! command; design D14).
//!
//! A command line from a tool that spawns processes goes through one fixed
//! order, [`evaluate`]:
//!
//! 1. hardline commands ([`hardline`]) are blocked, always;
//! 2. user deny rules (`approvals.deny`) are blocked, always;
//! 3. bypass (`approvals.mode = "off"`, `--yolo`, `/yolo`) allows;
//! 4. allow rules (`[approvals].allow` in `grants.toml`, session
//!    approvals) allow;
//! 5. dangerous patterns ([`dangerous`]) and content detectors
//!    ([`detectors`]) need a decision;
//! 6. anything else is allowed.
//!
//! [`CommandGate`] applies the decision step: an interactive session asks
//! the human (`once`, `session`, `always`, `deny`; no answer, a detached
//! surface or no channel at all deny), an unattended context (cron,
//! single-query, webhook/API) answers at once from its typed mode. The same
//! gate asks for writes to protected instruction files in every mode and
//! for plugin escalations (`plugin_rule:<key>`). `prometheus approvals
//! test` runs [`evaluate`] without executing or persisting anything.
//!
//! Self-termination guards (daemon-hosted sessions) and the smart guardian
//! are M2: in M1 `smart` asks the human exactly like `manual`, which is
//! what the guardian does when it cannot decide.

pub mod dangerous;
pub mod detectors;
mod gate;
pub mod hardline;
pub mod paths;
pub mod rules;
pub mod shell;

use std::fmt;
use std::path::Path;

use serde::Serialize;

pub use gate::{ApprovalRequest, CommandGate, Decision, GateSetup, STOP};
pub use shell::ParseError;

/// Where a session runs, which decides who answers an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Context {
    /// A human can answer (CLI, TUI, desktop, chat platforms).
    Interactive,
    /// A cron job (`approvals.cron_mode`).
    Cron,
    /// `prometheus chat -z` (`approvals.single_query_mode`).
    SingleQuery,
    /// Webhook and API surfaces without an approval channel
    /// (`approvals.unattended_mode`).
    Unattended,
}

impl Context {
    /// Every context, for argument parsing.
    pub const ALL: [Self; 4] = [
        Self::Interactive,
        Self::Cron,
        Self::SingleQuery,
        Self::Unattended,
    ];

    /// The name used in config keys and messages.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Cron => "cron",
            Self::SingleQuery => "single_query",
            Self::Unattended => "unattended",
        }
    }
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The paths hardline rules protect, as `/`-separated text, and the
/// working directory relative targets resolve against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Floors {
    /// The command's working directory.
    pub cwd: String,
    /// The user's home directory.
    pub user_home: Option<String>,
    /// `PROMETHEUS_HOME`.
    pub prometheus_home: Option<String>,
    /// The running Prometheus executable.
    pub binary: Option<String>,
}

impl Floors {
    /// Floors for commands run in `cwd`.
    #[must_use]
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: paths::text(cwd),
            ..Self::default()
        }
    }

    /// Sets the user home.
    #[must_use]
    pub fn user_home(mut self, path: Option<&Path>) -> Self {
        self.user_home = path.map(paths::text);
        self
    }

    /// Sets the Prometheus home.
    #[must_use]
    pub fn prometheus_home(mut self, path: &Path) -> Self {
        self.prometheus_home = Some(paths::text(path));
        self
    }

    /// Sets the running binary (normally `std::env::current_exe()`).
    #[must_use]
    pub fn binary(mut self, path: Option<&Path>) -> Self {
        self.binary = path.map(paths::text);
        self
    }
}

/// Why a command needs a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// `dangerous-pattern`, `secret-upload`, `invisible-characters`,
    /// `protected-file` or `plugin_rule:<key>`.
    pub detector: String,
    /// What the pattern is.
    pub reason: String,
    /// The offending part of the command, invisible characters escaped.
    pub fragment: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.detector, self.reason, self.fragment)
    }
}

/// The outcome of the gate order for one command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Always blocked: the hardline reason.
    Hardline(String),
    /// Always blocked: the deny rule.
    DenyRule(String),
    /// Allowed by `off` / yolo.
    Bypass,
    /// Allowed by an allow rule.
    Allowlisted(String),
    /// Needs a decision for these findings.
    Prompt(Vec<Finding>),
    /// Nothing matched.
    Allowed,
}

impl Verdict {
    /// The verdict name `approvals test` prints.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Hardline(_) => "hardline",
            Self::DenyRule(_) => "deny rule",
            Self::Bypass => "bypass",
            Self::Allowlisted(_) => "allowlisted",
            Self::Prompt(_) => "would prompt",
            Self::Allowed => "allowed",
        }
    }

    /// The tool result text of a blocked verdict.
    #[must_use]
    pub fn blocked_message(&self) -> Option<String> {
        match self {
            Self::Hardline(reason) => Some(format!("BLOCKED (hardline): {reason}")),
            Self::DenyRule(rule) => Some(format!("BLOCKED (deny rule '{rule}')")),
            _ => None,
        }
    }
}

/// What [`evaluate`] checks a command against.
#[derive(Debug, Clone, Copy)]
pub struct Policy<'a> {
    /// `off` mode or yolo.
    pub bypass: bool,
    /// `approvals.deny`.
    pub deny: &'a [String],
    /// `[approvals].allow` plus session approvals.
    pub allow: &'a [String],
    /// Protected paths and the working directory.
    pub floors: &'a Floors,
}

/// Runs `line` through the gate order without executing anything.
///
/// # Errors
///
/// The command line cannot be read (an unterminated quote...).
pub fn evaluate(line: &str, policy: &Policy<'_>) -> Result<Verdict, ParseError> {
    let script = shell::parse(line)?;
    if let Some(reason) = hardline::check(line, &script, policy.floors) {
        return Ok(Verdict::Hardline(reason));
    }
    if let Some(rule) = rules::denied(policy.deny, line, &script) {
        return Ok(Verdict::DenyRule(rule.to_owned()));
    }
    if policy.bypass {
        return Ok(Verdict::Bypass);
    }
    if let Some(rule) = rules::allowed(policy.allow, line, &script) {
        return Ok(Verdict::Allowlisted(rule.to_owned()));
    }
    let mut findings = dangerous::findings(line, &script, policy.floors);
    findings.extend(detectors::findings(line, &script));
    Ok(if findings.is_empty() {
        Verdict::Allowed
    } else {
        Verdict::Prompt(findings)
    })
}

#[cfg(test)]
mod tests;
