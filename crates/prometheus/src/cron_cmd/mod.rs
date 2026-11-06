//! `prometheus cron` (spec: scheduled-jobs, CLI surface; Lifecycle
//! operations; Canonical schedule echo and confirmation; Run history; Cron
//! doctor; Headless tick without a daemon). A job is named by its id or
//! unique name; an ambiguous name exits 2 listing the ids.

mod args;
mod diff;
mod edit;
mod show;
mod tick;

use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_store::Store;

pub use self::args::{CronCommand, JobFlags};

use crate::context::Context;
use crate::cron_host::HostRunner;

/// What every subcommand works with.
pub(crate) struct Env<'a> {
    pub ctx: &'a Context<'a>,
    pub scope: ProfileScope,
    pub store: Arc<Store>,
}

impl Env<'_> {
    pub(crate) fn runner(&self) -> HostRunner {
        HostRunner::new(&self.scope, self.ctx.launch, Arc::clone(&self.store))
    }
}

/// Runs one `cron` subcommand.
///
/// # Errors
///
/// Unknown jobs (66), ambiguous names (2), invalid schedules and jobs
/// (65), store errors.
pub fn run(command: &CronCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let store = Arc::new(Store::open(&scope)?);
    let env = Env { ctx, scope, store };
    let now = Timestamp::now();
    match command {
        CronCommand::List { all } => show::list(&env, *all, now),
        CronCommand::Add { schedule, flags } => edit::add(&env, schedule.as_deref(), flags, now),
        CronCommand::Edit {
            job,
            schedule,
            flags,
        } => edit::edit(&env, job, schedule.as_deref(), flags, now),
        CronCommand::Pause { job, reason } => edit::pause(&env, job, reason.as_deref(), now),
        CronCommand::Resume { job } => edit::resume(&env, job, now),
        CronCommand::Remove { job } => edit::remove(&env, job),
        CronCommand::Run { job, queue } => tick::run_now(&env, job, *queue, now),
        CronCommand::Runs { job, limit, status } => {
            show::runs(&env, job, *limit, status.as_deref())
        }
        CronCommand::ShowRun { run } => show::show_run(&env, run),
        CronCommand::Replay { run, live, deliver } => tick::replay(&env, run, *live, *deliver),
        CronCommand::Status => show::status(&env, now),
        CronCommand::Doctor => show::doctor(&env, now),
        CronCommand::Tick => tick::tick(&env),
    }
}
