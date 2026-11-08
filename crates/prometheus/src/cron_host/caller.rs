//! What a model session may hand to a job it schedules through the
//! `cronjob` tool (spec: scheduled-jobs, Model tool surface; Script wake
//! gate and script credentials). The tool's `confirm_token` comes back to
//! the model, so it is no human approval: a job the model sets up runs
//! unattended and may not reach past the session that asked for it. Its
//! toolsets stay within the session's tools, its script secrets need the
//! session's own `secrets.read` grants, and its interpreter is never a
//! file the session can write (the model would choose the code that runs
//! with those secrets). A person widens a job with `prometheus cron
//! add|edit`.

use std::collections::BTreeSet;
use std::path::Path;

use pmt_core::capability::{Access, Capability, CapabilitySet, Places};
use pmt_core::error::{Error, codes};
use pmt_cron::fields::Fields;

/// The calling session's reach.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    /// The session's effective tool surface (tool names).
    pub tools: BTreeSet<String>,
    /// The session's effective capabilities.
    pub capabilities: CapabilitySet,
    /// The placeholders of those capabilities (its workspace, `~`).
    pub places: Places,
}

impl Caller {
    /// Refuses `fields` that ask for more than the session holds.
    ///
    /// # Errors
    ///
    /// `cron.beyond_session` naming what is out of reach.
    pub fn check(&self, fields: &Fields) -> Result<(), Error> {
        if let Some(toolsets) = &fields.toolsets {
            self.check_toolsets(toolsets)?;
        }
        if let Some(secrets) = &fields.script_secrets {
            self.check_secrets(secrets)?;
        }
        match fields.interpreter.as_deref() {
            Some(interpreter) if !interpreter.is_empty() => self.check_interpreter(interpreter),
            _ => Ok(()),
        }
    }

    fn check_toolsets(&self, toolsets: &[String]) -> Result<(), Error> {
        // An unknown toolset is the shared validation's to report.
        let Ok(tools) = crate::tools_cmd::expand_toolsets(toolsets) else {
            return Ok(());
        };
        let missing: Vec<&str> = tools
            .iter()
            .filter(|tool| !self.tools.contains(*tool))
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        Err(beyond(&format!(
            "toolsets {} give tools this session lacks ({})",
            toolsets.join(", "),
            missing.join(", ")
        )))
    }

    fn check_secrets(&self, secrets: &[String]) -> Result<(), Error> {
        let missing: Vec<&str> = secrets
            .iter()
            .filter(|name| !name.is_empty())
            .filter(|name| {
                !self
                    .capabilities
                    .allows(&Capability::SecretsRead((*name).clone()))
            })
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        Err(beyond(&format!(
            "script_secrets {} need this session's secrets.read grant",
            missing.join(", ")
        )))
    }

    /// The path as given and the file it resolves to are both checked: a
    /// writable link could be repointed after the job is saved.
    fn check_interpreter(&self, interpreter: &str) -> Result<(), Error> {
        let path = Path::new(interpreter);
        let workspace = &self.places.workspace;
        let writable = |p: &Path| {
            (!workspace.as_os_str().is_empty() && p.starts_with(workspace))
                || self
                    .capabilities
                    .allows_path(Access::Write, p, &self.places)
        };
        let resolved = path.exists().then(|| pmt_core::capability::canonical(path));
        if writable(path) || resolved.as_deref().is_some_and(writable) {
            return Err(beyond(&format!(
                "interpreter {interpreter} is a file this session can write"
            )));
        }
        Ok(())
    }
}

fn beyond(what: &str) -> Error {
    Error::new(
        &codes::CRON_BEYOND_SESSION,
        format!("{what}; a person can set it with `prometheus cron add|edit`"),
    )
}
