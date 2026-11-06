//! `cron list|runs|show-run|status|doctor`.

use std::fmt::Write as _;

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_cron::doctor;
use pmt_cron::job::{Job, JobState};
use pmt_cron::ops;
use pmt_cron::schedule::at_ms;
use pmt_store::cron::RunRow;
use serde_json::{Value, json};

use super::Env;
use crate::cron_host::tool::summary;
use crate::output::{document, print_raw};

fn when(ms: Option<i64>) -> String {
    ms.map_or_else(
        || "-".to_owned(),
        |ms| at_ms(ms).strftime("%Y-%m-%d %H:%M:%SZ").to_string(),
    )
}

pub(super) fn list(env: &Env<'_>, all: bool, _now: Timestamp) -> Result<u8, Error> {
    let jobs: Vec<Job> = env
        .store
        .cron_jobs()?
        .into_iter()
        .filter(|row| all || row.state != JobState::Completed.as_str())
        .map(Job::from_row)
        .collect::<Result<_, _>>()?;
    if env.ctx.json {
        let jobs: Vec<Value> = jobs.iter().map(summary).collect();
        return document("cron list", &json!({"jobs": jobs}));
    }
    if jobs.is_empty() {
        return print_raw("no cron jobs\n");
    }
    let mut text = String::new();
    for job in &jobs {
        let _ = writeln!(
            text,
            "{}  {:<24}  {:<10}  {} {}  next {}  last {}",
            job.id(),
            job.row.name,
            job.row.state,
            job.spec.schedule.kind(),
            job.spec.schedule.canonical(),
            when(job.row.next_run_at),
            job.row.last_status.as_deref().unwrap_or("-"),
        );
        if let Some(reason) = &job.row.paused_reason {
            let _ = writeln!(text, "    paused: {reason}");
        }
        if let Some(error) = job
            .row
            .last_error
            .as_deref()
            .filter(|_| job.state() == JobState::Error)
        {
            let _ = writeln!(text, "    error: {error}");
        }
    }
    print_raw(&text)
}

fn duration(run: &RunRow) -> String {
    match (run.started_at, run.ended_at) {
        (Some(start), Some(end)) => format!(
            "{:.1}s",
            f64::from(i32::try_from(end - start).unwrap_or(i32::MAX)) / 1000.0
        ),
        _ => "-".to_owned(),
    }
}

fn run_json(run: &RunRow) -> Value {
    json!({
        "run_id": run.id,
        "job_id": run.job_id,
        "job_name": run.job_name,
        "scheduled_at": at_ms(run.scheduled_at).to_string(),
        "trigger": run.trigger,
        "lateness": run.lateness,
        "collapsed": run.collapsed,
        "skipped": run.skipped,
        "status": run.status,
        "reason": run.reason,
        "error": run.error,
        "started_at": run.started_at.map(|t| at_ms(t).to_string()),
        "ended_at": run.ended_at.map(|t| at_ms(t).to_string()),
        "duration": duration(run),
        "session_id": run.session_id,
        "input_tokens": run.input_tokens,
        "output_tokens": run.output_tokens,
        "cost_usd": run.cost_usd,
        "delivered": run.delivered.as_deref().and_then(|d| serde_json::from_str::<Value>(d).ok()),
        "output": run.output,
        "output_blob": run.output_blob,
    })
}

pub(super) fn runs(
    env: &Env<'_>,
    key: &str,
    limit: usize,
    status: Option<&str>,
) -> Result<u8, Error> {
    let job = ops::find(&env.store, key)?;
    let runs = env.store.cron_runs(job.id(), status, limit.max(1))?;
    if env.ctx.json {
        let runs: Vec<Value> = runs.iter().map(run_json).collect();
        return document("cron runs", &json!({"job": job.id(), "runs": runs}));
    }
    if runs.is_empty() {
        return print_raw(&format!("no runs of job {}\n", job.id()));
    }
    let mut text = String::new();
    for run in &runs {
        let _ = writeln!(
            text,
            "{}  {}  {:<15}  {:>7}  ${:.4}  {}{}",
            run.id,
            when(Some(run.scheduled_at)),
            run.status,
            duration(run),
            run.cost_usd,
            run.session_id.as_deref().unwrap_or("-"),
            run.error
                .as_deref()
                .map(|e| format!("  {e}"))
                .unwrap_or_default(),
        );
    }
    print_raw(&text)
}

pub(super) fn show_run(env: &Env<'_>, id: &str) -> Result<u8, Error> {
    let run = env
        .store
        .cron_run(id)?
        .ok_or_else(|| pmt_store::cron::runs::run_not_found(id))?;
    let mut doc = run_json(&run);
    if let Some(sha) = &run.output_blob
        && let Some(blob) = pmt_store::BlobRef::parse(sha)
        && let Ok(bytes) = env.store.get_blob(&blob)
    {
        doc["output"] = Value::String(String::from_utf8_lossy(&bytes).into_owned());
    }
    if env.ctx.json {
        return document("cron show-run", &doc);
    }
    let mut text = String::new();
    for key in [
        "run_id",
        "job_id",
        "job_name",
        "scheduled_at",
        "trigger",
        "lateness",
        "collapsed",
        "skipped",
        "status",
        "reason",
        "error",
        "started_at",
        "ended_at",
        "duration",
        "session_id",
        "input_tokens",
        "output_tokens",
        "cost_usd",
        "delivered",
    ] {
        let value = &doc[key];
        if !value.is_null() {
            let shown = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned);
            let _ = writeln!(text, "{key}: {shown}");
        }
    }
    if let Some(output) = doc["output"].as_str() {
        let _ = write!(text, "output:\n{output}\n");
    }
    print_raw(&text)
}

pub(super) fn status(env: &Env<'_>, now: Timestamp) -> Result<u8, Error> {
    let jobs = env.store.cron_jobs()?;
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for job in &jobs {
        *counts.entry(job.state.clone()).or_default() += 1;
    }
    let next = jobs
        .iter()
        .filter(|j| j.enabled)
        .filter_map(|j| j.next_run_at.map(|at| (at, j)))
        .min_by_key(|(at, _)| *at);
    let report = doctor::report(&env.scope, &env.store, now)?;
    let doc = json!({
        "profile": report.profile,
        "provider": report.provider,
        "jobs": counts,
        "next": next.map(|(at, j)| json!({"id": j.id, "name": j.name, "at": at_ms(at).to_string()})),
        "lease": report.lease,
    });
    if env.ctx.json {
        return document("cron status", &doc);
    }
    let mut text = format!("profile {}: trigger {}\n", report.profile, report.provider);
    let counted: Vec<String> = counts.iter().map(|(s, n)| format!("{n} {s}")).collect();
    let _ = writeln!(
        text,
        "jobs: {}",
        if counted.is_empty() {
            "none".to_owned()
        } else {
            counted.join(", ")
        }
    );
    if let Some((at, job)) = next {
        let _ = writeln!(
            text,
            "next: {} ({}) at {}",
            job.id,
            job.name,
            when(Some(at))
        );
    }
    let _ = writeln!(text, "{}", lease_line(report.lease.as_ref()));
    print_raw(&text)
}

fn lease_line(lease: Option<&doctor::Holder>) -> String {
    match lease {
        Some(h) => format!("tick lease: {} pid {} on {}", h.kind, h.pid, h.host_id),
        None => "tick lease: free (no daemon ticks this profile)".to_owned(),
    }
}

pub(super) fn doctor(env: &Env<'_>, now: Timestamp) -> Result<u8, Error> {
    let report = doctor::report(&env.scope, &env.store, now)?;
    let code = u8::from(!report.healthy());
    if env.ctx.json {
        document("cron doctor", &report)?;
        return Ok(code);
    }
    let mut text = format!(
        "profile {}\ntrigger: {} (configured {})\n",
        report.profile, report.provider, report.configured_provider
    );
    if let Some(reason) = &report.fallback {
        let _ = writeln!(text, "fallback: {reason}");
    }
    let _ = writeln!(text, "{}", lease_line(report.lease.as_ref()));
    let _ = writeln!(text, "due but not fired: {}", report.due_unfired);
    for problem in &report.problems {
        let _ = writeln!(
            text,
            "{}: {} ({}) {}",
            problem.kind,
            problem.id,
            problem.name,
            problem.detail.as_deref().unwrap_or("")
        );
    }
    for row in &report.undelivered {
        let _ = writeln!(
            text,
            "undelivered: run {} to {} is {}{}",
            row.run_id,
            row.target,
            row.state,
            row.error
                .as_deref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default()
        );
    }
    let _ = writeln!(text, "settings:");
    for setting in &report.settings {
        let _ = writeln!(
            text,
            "  {} = {} ({})",
            setting.key, setting.value, setting.source
        );
    }
    print_raw(&text)?;
    Ok(code)
}
