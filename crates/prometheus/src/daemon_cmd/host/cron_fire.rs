//! Scheduled jobs fired by a webhook route (spec: scheduled-jobs,
//! Webhook-triggered job runs): the run is queued through the same claim
//! as a manual run (`webhook` trigger, the rendered event as its transient
//! run context), exclusive so a burst starts one run, and the profile's
//! scheduler loop is woken to start it now. A paused job or one already
//! running drops the event.

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_cron::ops;
use pmt_gateway::messaging::CronFire;

use super::DaemonHost;

impl DaemonHost {
    /// The id of `profile`'s job `key` (id or name).
    pub(super) fn cron_job_id(&self, profile: &str, key: &str) -> Result<String, Error> {
        let profile = self.profile(profile)?;
        Ok(ops::find(&profile.store, key)?.id().to_owned())
    }

    /// Queues a webhook run of `profile`'s job `key` with `context`.
    pub(super) fn fire_cron(
        &self,
        profile: &str,
        key: &str,
        context: &str,
    ) -> Result<CronFire, Error> {
        let served = self.profile(profile)?;
        let how = ops::QueueRun {
            trigger: Some("webhook"),
            context: Some(context.to_owned()),
            exclusive: true,
            enabled_only: true,
        };
        match ops::queue_run(&served.store, key, Timestamp::now(), &how) {
            Ok((job, run)) => {
                served.cron_wake.notify_one();
                Ok(CronFire::Queued {
                    job_id: job.id().to_owned(),
                    run_id: run.id,
                })
            }
            Err(err)
                if err.code().code == codes::CRON_ALREADY_RUNNING.code
                    || err.code().code == codes::CRON_INVALID_STATE.code =>
            {
                Ok(CronFire::Dropped {
                    reason: err.message().to_owned(),
                })
            }
            Err(err) => Err(err),
        }
    }
}
