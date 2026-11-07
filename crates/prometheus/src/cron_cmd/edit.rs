//! `cron add|edit|pause|resume|remove`: every create and schedule change
//! shows the canonical echo first and saves only once confirmed (`y` on a
//! terminal, or `--yes`; without a terminal `--yes` is required, exit 4).

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_cron::job::Job;
use pmt_cron::ops;
use pmt_cron::schedule::Echo;
use serde_json::json;

use super::{Env, JobFlags};
use crate::cron_host::{profile_zone, scripts_dir};
use crate::output::{document, print_error, print_raw};

/// Shows `echo` and asks; `false` (after printing the refusal) when the
/// job must not be saved.
fn confirmed(env: &Env<'_>, echo: &Echo, yes: bool) -> Result<bool, Error> {
    if !env.ctx.json {
        println!("{}", echo.render());
    }
    if yes {
        return Ok(true);
    }
    if !crate::prompt::stdin_is_tty() {
        let err = Error::new(
            &codes::CRON_CONFIRMATION,
            "refusing to save without confirmation: pass --yes",
        );
        print_error(&err, env.ctx.json);
        return Ok(false);
    }
    crate::prompt::confirm("Save this job?")
}

fn saved(env: &Env<'_>, verb: &str, job: &Job, echo: Option<&Echo>) -> Result<u8, Error> {
    if env.ctx.json {
        return document(
            "cron add",
            &json!({"job": crate::cron_host::tool::summary(job), "echo": echo}),
        );
    }
    print_raw(&format!("{verb} job {} ({})\n", job.id(), job.row.name))
}

pub(super) fn add(
    env: &Env<'_>,
    schedule: Option<&str>,
    flags: &JobFlags,
    paused: Option<String>,
    now: Timestamp,
) -> Result<u8, Error> {
    let runner = env.runner();
    let scripts = scripts_dir(&env.scope);
    let profile = runner.profile(&scripts);
    let spec = flags
        .fields(schedule, &env.scope)?
        .new_spec(now, &profile.zone)?;
    let mut checked = spec.clone();
    if checked.deliver.is_empty() {
        checked.deliver = ops::default_deliver(None);
    }
    let echo = ops::check(&profile, &checked, now)?;
    if !confirmed(env, &echo, flags.yes)? {
        return Ok(pmt_core::exit::APPROVAL_REQUIRED);
    }
    let new = ops::NewJob {
        name: flags.name.clone(),
        origin: None,
        paused,
    };
    let job = ops::create(&profile, spec, new, now)?;
    saved(env, "created", &job, Some(&echo))
}

pub(super) fn edit(
    env: &Env<'_>,
    key: &str,
    schedule: Option<&str>,
    flags: &JobFlags,
    now: Timestamp,
) -> Result<u8, Error> {
    let runner = env.runner();
    let scripts = scripts_dir(&env.scope);
    let profile = runner.profile(&scripts);
    let fields = flags.fields(schedule, &env.scope)?;
    if fields.reschedules() {
        let mut spec = ops::find(&env.store, key)?.spec;
        fields.apply(&mut spec, now, &profile.zone)?;
        let echo = ops::check(&profile, &spec, now)?;
        if !confirmed(env, &echo, flags.yes)? {
            return Ok(pmt_core::exit::APPROVAL_REQUIRED);
        }
    }
    let (job, echo) = ops::update(
        &profile,
        key,
        flags.name.as_deref(),
        |spec| fields.apply(spec, now, &profile.zone),
        now,
    )?;
    saved(env, "updated", &job, echo.as_ref())
}

pub(super) fn pause(
    env: &Env<'_>,
    key: &str,
    reason: Option<&str>,
    now: Timestamp,
) -> Result<u8, Error> {
    let job = ops::pause(&env.store, key, reason, now)?;
    print_raw(&format!("paused job {} ({})\n", job.id(), job.row.name))
}

pub(super) fn resume(env: &Env<'_>, key: &str, now: Timestamp) -> Result<u8, Error> {
    let job = ops::resume(&env.store, key, now, &profile_zone(&env.scope)?)?;
    let next = job
        .next_run()
        .map_or_else(|| "-".to_owned(), |t| t.to_string());
    print_raw(&format!(
        "resumed job {} ({}); next run {next}\n",
        job.id(),
        job.row.name
    ))
}

pub(super) fn remove(env: &Env<'_>, key: &str) -> Result<u8, Error> {
    let job = ops::remove(&env.store, key)?;
    print_raw(&format!("removed job {} ({})\n", job.id(), job.row.name))
}
