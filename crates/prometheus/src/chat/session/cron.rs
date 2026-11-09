//! What makes a session a scheduled run (spec: scheduled-jobs, Fresh
//! isolated agent session per run; Memory policy for unattended runs;
//! Unattended approvals fail closed): `source = cron` with the job as its
//! origin, the job's toolsets (never `clarify` or other interactive tools,
//! `cronjob` only with `cron.allow_agent_scheduling`), no memory write
//! tool unless the job opts in, and approvals decided by
//! `approvals.cron_mode` (the gate's cron context).

use std::path::{Path, PathBuf};

use pmt_core::capability::{Capability, CapabilitySet};
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
    /// The run's notepad channel directory
    /// ([`crate::cron_host::notepad`]): writable by its terminal, whose
    /// `PATH` starts with the channel's `bin/`.
    pub notepad: Option<PathBuf>,
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

    /// What the run's sandbox adds for its notepad channel: writes of the
    /// channel directory and reads of the running executable (the link in
    /// the channel's `bin/` points at it).
    #[must_use]
    pub fn sandbox_grants(&self) -> CapabilitySet {
        let Some(dir) = &self.notepad else {
            return CapabilitySet::default();
        };
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
        std::iter::once(Capability::write_under(&dir))
            .chain(crate::cron_host::notepad::executable().map(|exe| Capability::read_under(&exe)))
            .collect()
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

/// What a job the session's model schedules through `cronjob` may not
/// reach past: the session's selected tools and its capabilities in its
/// working directory `cwd`.
#[must_use]
pub fn caller(
    selection: &Selection,
    capabilities: &CapabilitySet,
    cwd: &Path,
    user_home: Option<&PathBuf>,
) -> crate::cron_host::caller::Caller {
    crate::cron_host::caller::Caller {
        tools: selection.tools.iter().cloned().collect(),
        capabilities: capabilities.clone(),
        places: pmt_core::capability::Places {
            user_home: user_home.cloned().unwrap_or_default(),
            workspace: pmt_core::capability::canonical(cwd),
            scratch: Vec::new(),
            toolchain: Vec::new(),
        },
    }
}
