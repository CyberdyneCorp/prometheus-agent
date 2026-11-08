//! Retired and deprecated models pinned by stored cron jobs (spec:
//! configuration-and-profiles, Retired model rewrite): `config fix-models`
//! and `doctor` report them next to the config keys, and `--apply`
//! rewrites each pin through the job edit path (re-validated like
//! `prometheus cron edit --model`).

use jiff::Timestamp;
use pmt_config::Config;
use pmt_core::error::Error;
use pmt_cron::job::Job;
use pmt_providers::ProviderRegistry;
use pmt_providers::retirement::{self, PinFinding};
use pmt_store::Store;

use super::HostRunner;

/// One job's retired or deprecated pin.
#[derive(Debug, Clone)]
pub struct JobPin {
    /// The job id.
    pub job_id: String,
    /// The finding (key `cron job <name> (<id>).model`) and its rewrite.
    pub pin: PinFinding,
}

/// Every stored job whose pinned model is deprecated or retired.
///
/// # Errors
///
/// Store errors.
pub fn scan(
    config: &Config,
    registry: &ProviderRegistry,
    store: &Store,
) -> Result<Vec<JobPin>, Error> {
    let mut pins = Vec::new();
    for row in store.cron_jobs()? {
        // An unreadable definition is reported by `cron doctor`, not here.
        let Ok(job) = Job::from_row(row) else {
            continue;
        };
        let Some(model) = job.spec.model.as_deref() else {
            continue;
        };
        let key = format!("cron job {} ({}).model", job.row.name, job.id());
        if let Some(pin) =
            retirement::scan_pin(config, registry, key, model, job.spec.provider.as_deref())
        {
            pins.push(JobPin {
                job_id: job.id().to_owned(),
                pin,
            });
        }
    }
    Ok(pins)
}

/// Rewrites every pin of `pins` that has a replacement; returns how many
/// jobs changed.
///
/// # Errors
///
/// Store errors and the job validation of `cron edit`.
pub fn apply(runner: &HostRunner, pins: &[JobPin]) -> Result<usize, Error> {
    let scripts = super::scripts_dir(runner.scope());
    let profile = runner.profile(&scripts);
    let mut changed = 0;
    for pin in pins {
        let Some((model, provider)) = &pin.pin.rewrite else {
            continue;
        };
        let edit = |spec: &mut pmt_cron::job::JobSpec| {
            spec.model = Some(model.clone());
            if let Some(provider) = provider {
                spec.provider = Some(provider.clone());
            }
            Ok(())
        };
        pmt_cron::ops::update(&profile, &pin.job_id, None, edit, Timestamp::now())?;
        changed += 1;
    }
    Ok(changed)
}
