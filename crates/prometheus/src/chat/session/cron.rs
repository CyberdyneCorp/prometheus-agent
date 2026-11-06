//! What makes a session a scheduled run (spec: scheduled-jobs, Fresh
//! isolated agent session per run; Memory policy for unattended runs;
//! Unattended approvals fail closed): `source = cron` with the job as its
//! origin, the job's toolsets (never `clarify` or other interactive tools,
//! `cronjob` only with `cron.allow_agent_scheduling`), no memory write
//! tool unless the job opts in, and approvals decided by
//! `approvals.cron_mode` (the gate's cron context).

use pmt_core::error::Error;
use pmt_store::{NewSession, Origin, Source};
use pmt_tools::{Selection, ToolRegistry};

/// A scheduled run's session settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronRun {
    /// The job.
    pub job_id: String,
    /// The run's toolsets (the job's, else `cron.default_toolsets`);
    /// `None` keeps the cron surface's default.
    pub toolsets: Option<Vec<String>>,
    /// `memory = read_write`: the memory tool is offered.
    pub memory_write: bool,
    /// `cron.allow_agent_scheduling`.
    pub allow_scheduling: bool,
}

/// The tool of scheduling jobs.
const CRONJOB: &str = "cronjob";
/// The memory write tool.
const MEMORY: &str = "memory";

impl CronRun {
    /// The session row of a run.
    #[must_use]
    pub fn new_session(&self) -> NewSession {
        let mut new = NewSession::new(Source::Cron);
        new.origin = Origin::Cron {
            job_id: self.job_id.clone(),
        };
        new
    }

    /// The run's tools: `base` (the cron surface's default) or the job's
    /// toolsets, without interactive tools, `cronjob` unless allowed and
    /// `memory` unless the job may write it.
    ///
    /// # Errors
    ///
    /// An unknown toolset (`registry.unknown_name`).
    pub fn selection(
        &self,
        registry: &ToolRegistry,
        disabled: &[String],
        base: Selection,
    ) -> Result<Selection, Error> {
        let selection = match &self.toolsets {
            Some(toolsets) => Selection::resolve(registry, toolsets, disabled)?,
            None => base,
        };
        let mut selection = selection.without_interactive(registry);
        let allow_scheduling = self.allow_scheduling;
        let memory_write = self.memory_write;
        let dropped: Vec<String> = selection
            .tools
            .iter()
            .filter(|t| {
                (t.as_str() == CRONJOB && !allow_scheduling)
                    || (t.as_str() == MEMORY && !memory_write)
            })
            .cloned()
            .collect();
        selection.tools.retain(|t| !dropped.contains(t));
        selection.disabled.extend(dropped);
        Ok(selection)
    }
}
