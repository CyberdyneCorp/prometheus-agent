//! `cron run|replay|tick`: the foreground run, the live replay and the
//! headless tick (spec: scheduled-jobs, Lifecycle operations; Run replay;
//! Headless tick without a daemon).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_cron::ops;
use pmt_store::cron::{CronLease, RunRow};
use pmt_store::leases::Holder;
use serde_json::json;

use super::Env;
use crate::cron_host::{self, HostRunner, scripts_dir};
use crate::output::{document, print_raw};

/// How long a headless tick's lease lives between renewals.
const TICK_LEASE: Duration = Duration::from_mins(2);

fn finished(run: &RunRow) -> u8 {
    match run.status.as_str() {
        "ok" | "silent" | "delivery_queued" => 0,
        _ => pmt_core::exit::FAILURE,
    }
}

fn report(run: &RunRow) -> String {
    let mut text = format!("run {}: {}", run.id, run.status);
    if let Some(error) = &run.error {
        let _ = write!(text, " ({error})");
    }
    text.push('\n');
    if let Some(output) = &run.output {
        text.push_str(output.trim_end());
        text.push('\n');
    }
    text
}

pub(super) fn run_now(env: &Env<'_>, key: &str, queue: bool, now: Timestamp) -> Result<u8, Error> {
    let (job, run) = ops::manual_run(&env.store, key, now)?;
    if queue {
        let note = if crate::daemon_client::running(env.scope.home())?.is_some() {
            "the gateway daemon runs it at its next tick"
        } else {
            "no gateway daemon runs: it executes at the next `prometheus cron tick` or daemon start"
        };
        return print_raw(&format!(
            "queued run {} of job {}; {note}\n",
            run.id,
            job.id()
        ));
    }
    let runner = env.runner();
    let done = crate::model_cmd::block_on(runner.execute(&job, &run))??;
    print_raw(&report(&done))?;
    Ok(finished(&done))
}

pub(super) fn replay(env: &Env<'_>, id: &str, live: bool, deliver: bool) -> Result<u8, Error> {
    let original = env
        .store
        .cron_run(id)?
        .ok_or_else(|| pmt_store::cron::runs::run_not_found(id))?;
    if !live {
        return crate::replay::run::cron(&env.scope, &env.store, env.ctx.launch, &original);
    }
    let job = ops::find(&env.store, &original.job_id)?;
    let runner = env.runner();
    let scripts = scripts_dir(&env.scope);
    let config = env.scope.config().cron.clone();
    let zone = crate::cron_host::profile_zone(&env.scope)?;
    let ctx = pmt_cron::exec::Exec {
        store: &env.store,
        profile: env.scope.name().as_str(),
        zone: &zone,
        scripts_dir: &scripts,
        config: &config,
        runner: &runner,
        clock: &pmt_cron::clock::SystemClock,
    };
    let new = crate::model_cmd::block_on(pmt_cron::exec::replay(&ctx, &job, &original, deliver))??;
    let old_text = original.output.as_deref().unwrap_or_default();
    let new_text = new.output.as_deref().unwrap_or_default();
    let mut text = format!(
        "replayed run {} as {} ({})\n",
        original.id, new.id, new.status
    );
    text.push_str(&pmt_cron::diff::unified(
        old_text,
        new_text,
        &original.id,
        &new.id,
    ));
    print_raw(&text)?;
    Ok(finished(&new))
}

pub(super) fn tick(env: &Env<'_>) -> Result<u8, Error> {
    let holder = Holder::current();
    let lease = match env.store.cron_acquire_lease(&holder, "tick", TICK_LEASE)? {
        CronLease::Granted(lease) => lease,
        CronLease::Busy(busy) => {
            let line = if busy.kind == "daemon" {
                format!(
                    "tick skipped: gateway daemon owns scheduling (pid {})",
                    busy.holder.pid
                )
            } else {
                format!("tick skipped: another tick runs (pid {})", busy.holder.pid)
            };
            if env.ctx.json {
                return document("cron tick", &json!({"skipped": line, "runs": []}));
            }
            return print_raw(&format!("{line}\n"));
        }
    };
    let store = Arc::clone(&env.store);
    let runner = Arc::new(env.runner());
    let outcome = crate::model_cmd::block_on(async move {
        let renew = tokio::spawn({
            let store = Arc::clone(&store);
            let lease = lease.clone();
            async move {
                loop {
                    tokio::time::sleep(TICK_LEASE / 4).await;
                    let _ = store.cron_renew_lease(&lease);
                }
            }
        });
        let result = headless(&store, runner).await;
        renew.abort();
        let _ = store.cron_release_lease(&lease);
        result
    })??;
    let runs: Vec<_> = outcome
        .iter()
        .map(|r| json!({"run_id": r.id, "job_id": r.job_id, "status": r.status, "error": r.error}))
        .collect();
    if env.ctx.json {
        return document("cron tick", &json!({"skipped": null, "runs": runs}));
    }
    let mut text = format!("tick: {} run(s)\n", outcome.len());
    for run in &outcome {
        text.push_str(&report(run));
    }
    print_raw(&text)
}

/// One tick of this process: crashed runs finalized, retention applied,
/// due runs executed; messaging deliveries stay queued for the daemon.
async fn headless(store: &pmt_store::Store, runner: Arc<HostRunner>) -> Result<Vec<RunRow>, Error> {
    pmt_cron::scheduler::recover(store)?;
    let now = Timestamp::now();
    pmt_cron::scheduler::prune(store, now, &runner.config())?;
    cron_host::drive(runner, now).await
}
