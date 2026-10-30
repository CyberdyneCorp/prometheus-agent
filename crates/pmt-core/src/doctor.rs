//! The result shape every `prometheus doctor` group returns
//! (spec: workspace-architecture, Doctor): each check reports `ok`, `warn`
//! or `fail` with a hint. Groups live with the code they check; the doctor
//! command only runs and prints them.

use serde::Serialize;

/// Outcome of one check. `--strict` turns `warn` into a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Nothing to do.
    Ok,
    /// Works, but something should change.
    Warn,
    /// Broken; doctor exits 1.
    Fail,
}

/// One doctor check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// The doctor group, for example `network`.
    pub group: &'static str,
    /// Stable check name within the group, for example `ipv6_route`.
    pub name: &'static str,
    /// The outcome.
    pub status: Status,
    /// One line describing what was found.
    pub message: String,
    /// The remedy, when there is one.
    pub hint: Option<String>,
}

impl Check {
    /// A check with no hint.
    #[must_use]
    pub fn new(
        group: &'static str,
        name: &'static str,
        status: Status,
        message: impl Into<String>,
    ) -> Self {
        Self {
            group,
            name,
            status,
            message: message.into(),
            hint: None,
        }
    }

    /// Adds the remedy.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}
