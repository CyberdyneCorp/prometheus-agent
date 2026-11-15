//! The scheduled trigger: a job with `kind = "evolve"` queues an
//! evolution job with `trigger = "cron"` and the scheduled job's id.

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_cron::job::Job;
use pmt_evolve::job::{self, Request, TaskInput, Trigger};

/// Queues the evolution job of scheduled job `job`; the queued job's id.
///
/// # Errors
///
/// `surface.not_included`, `evolve.disabled`, `evolve.invalid_task`.
pub fn queue(scope: &ProfileScope, job: &Job) -> Result<String, Error> {
    crate::surface::require("evolve")?;
    let task =
        job.spec.task.as_deref().ok_or_else(|| {
            Error::new(&codes::EVOLVE_INVALID_TASK, "the evolve job names no task")
        })?;
    let request = Request {
        cron_job_id: Some(job.id().to_owned()),
        ..Request::new(Trigger::Cron, TaskInput::File(scope.dir().join(task)))
    };
    job::queue(&super::profile(scope, None), &request).map(|j| j.id)
}
