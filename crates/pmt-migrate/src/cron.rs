//! The `cron` component (spec: hermes-migration, Scheduled jobs imported
//! paused).
//!
//! Each job of `cron/jobs.json` keeps its Hermes schedule semantics in the
//! Prometheus grammar (a bare `30m` is a recurring interval in Hermes, so
//! it becomes `every 30m`; one-shots stay one-shots; cron expressions are
//! unchanged), its referenced script is copied into `<profile>/scripts/`,
//! completed one-shots are skipped, and every job arrives paused unless
//! `--enable-cron` is given, so a job never fires from both agents.

use std::path::PathBuf;

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_cron::job::{JobSpec, MemoryMode};
use pmt_cron::ops::{self, NewJob, Profile};
use pmt_cron::schedule::Schedule;
use serde_json::Value;

use crate::component::Component;
use crate::ledger::{self, Decision};
use crate::plan::{Dest, Source, Step};
use crate::report::{Disposition, Entry};
use crate::writes;

/// Why an imported job is paused.
pub const PAUSED_REASON: &str =
    "Imported from Hermes paused; resume it once the Hermes scheduler is stopped.";

/// One job to create.
struct Job {
    name: String,
    spec: JobSpec,
    enabled: bool,
    script: Option<PathBuf>,
    entry: usize,
}

/// The planned cron component.
pub struct CronStep {
    entries: Vec<Entry>,
    jobs: Vec<Job>,
    enable: bool,
}

impl Step for CronStep {
    fn component(&self) -> Component {
        Component::Cron
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn warnings(&self) -> Vec<String> {
        if self.enable && self.jobs.iter().any(|j| j.enabled) {
            vec![
                "jobs enabled in Hermes are enabled here too: stop the Hermes scheduler, or each will fire twice"
                    .to_owned(),
            ]
        } else {
            Vec::new()
        }
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        let scripts = dest.scope.dir().join("scripts");
        let catalog = dest.host.cron_catalog(dest.scope);
        let zone = pmt_obs::time::profile_zone(&dest.scope.config().cron)?;
        let profile = Profile {
            store: dest.store,
            scripts_dir: &scripts,
            zone,
            catalog: catalog.as_ref(),
        };
        for job in &self.jobs {
            let outcome = import_job(&profile, job, self.enable, dest);
            let e = &mut self.entries[job.entry];
            match outcome {
                Ok(decision) => {
                    let why = format!("a job named '{}' exists with another definition", job.name);
                    writes::settle(decision, e, &why);
                }
                Err(err) => {
                    e.disposition = Disposition::Failed;
                    err.message().clone_into(&mut e.reason);
                }
            }
        }
        Ok(())
    }
}

/// The ledger digest of a stored job's definition.
fn stored_digest(store: &pmt_store::Store, name: &str) -> Option<String> {
    let job = ops::find(store, name).ok()?;
    Some(ledger::digest(
        pmt_cron::job::spec_json(&job.spec).as_bytes(),
    ))
}

/// Creates (or, while it still holds what the previous run wrote,
/// updates) one job as the ledger decides.
fn import_job(
    profile: &Profile<'_>,
    job: &Job,
    enable: bool,
    dest: &Dest<'_>,
) -> Result<Decision, Error> {
    let key = format!("job:{}", job.name);
    let hash = ledger::digest(pmt_cron::job::spec_json(&job.spec).as_bytes());
    let current = stored_digest(dest.store, &job.name);
    let prior = dest.ledger.borrow().get(&key).cloned();
    // A stored job never equals the Hermes definition byte for byte (the
    // store adds its own fields), so `Same` is only reached through the
    // ledger's written digest.
    let decision = match ledger::decide(
        prior.as_ref(),
        &hash,
        &hash,
        current.as_deref(),
        dest.overwrite,
    ) {
        Decision::Write if current.is_some() => {
            let row = ops::find(dest.store, &job.name)?.row;
            dest.on_undo(move |store| {
                store
                    .cron_modify_job(&row.id, |r| {
                        r.clone_from(&row);
                        Ok(())
                    })
                    .map(drop)
            });
            let spec = job.spec.clone();
            ops::update(
                profile,
                &job.name,
                None,
                |s| {
                    *s = spec;
                    Ok(())
                },
                Timestamp::now(),
            )?;
            Decision::Write
        }
        Decision::Write => {
            let script = copy_script(job, profile.scripts_dir, dest)?;
            let paused = (!(enable && job.enabled)).then(|| PAUSED_REASON.to_owned());
            let new = NewJob {
                name: Some(job.name.clone()),
                origin: None,
                paused,
            };
            let created = ops::create(profile, job.spec.clone(), new, Timestamp::now())
                .inspect_err(|_| {
                    // The job is a failed item: its script goes with it.
                    if let Some(script) = script {
                        let _ = std::fs::remove_file(script);
                    }
                })?;
            dest.on_undo(move |store| store.cron_delete_job(&created.row.id).map(drop));
            Decision::Write
        }
        other => other,
    };
    if decision == Decision::Write
        && let Some(written) = stored_digest(dest.store, &job.name)
    {
        dest.ledger.borrow_mut().record(key, hash, written);
    }
    Ok(decision)
}

/// Copies the job's script into `<profile>/scripts/` unless one is there;
/// returns the copy made.
fn copy_script(
    job: &Job,
    scripts: &std::path::Path,
    dest: &Dest<'_>,
) -> Result<Option<std::path::PathBuf>, Error> {
    let (Some(from), Some(rel)) = (&job.script, &job.spec.script) else {
        return Ok(None);
    };
    let to = scripts.join(rel);
    if to.exists() {
        return Ok(None);
    }
    dest.keep(&to)?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot create", parent, &e))?;
    }
    std::fs::copy(from, &to)
        .map(|_| Some(to.clone()))
        .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot write", &to, &e))
}

/// Plans the cron component.
///
/// # Errors
///
/// `migrate.component` when `cron/jobs.json` cannot be read or parsed.
pub fn plan(src: &Source<'_>) -> Result<CronStep, Error> {
    let mut step = CronStep {
        entries: Vec::new(),
        jobs: Vec::new(),
        enable: src.enable_cron,
    };
    let Some(text) = src.profile.read("cron/jobs.json")? else {
        return Ok(step);
    };
    let doc: Value = serde_json::from_str(&text).map_err(|e| {
        Error::new(
            &codes::MIGRATE_COMPONENT,
            format!("Hermes cron/jobs.json is not valid JSON: {e}"),
        )
    })?;
    let jobs = doc
        .get("jobs")
        .and_then(Value::as_array)
        .or_else(|| doc.as_array())
        .cloned()
        .unwrap_or_default();
    for job in &jobs {
        plan_job(src, job, &mut step);
    }
    Ok(step)
}

fn text<'a>(job: &'a Value, key: &str) -> Option<&'a str> {
    job.get(key)
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
}

fn plan_job(src: &Source<'_>, job: &Value, step: &mut CronStep) {
    let id = text(job, "id").unwrap_or("?");
    let name = text(job, "name").unwrap_or(id).to_owned();
    let item = format!("cron:{name}");
    let entry = |d| Entry::new(Component::Cron, item.clone(), d);
    let schedule = match schedule_of(job) {
        Ok(Some(s)) => s,
        Ok(None) => {
            step.entries
                .push(entry(Disposition::Skipped).because("completed one-shot"));
            return;
        }
        Err(why) => {
            step.entries.push(entry(Disposition::Failed).because(why));
            return;
        }
    };
    let (schedule, note) = schedule;
    let mut spec = JobSpec::new(schedule, text(job, "prompt").unwrap_or(""));
    spec.memory = MemoryMode::ReadOnly;
    spec.skills = strings(job.get("skills"));
    if spec.skills.is_empty()
        && let Some(skill) = text(job, "skill")
    {
        spec.skills.push(skill.to_owned());
    }
    spec.no_agent = job
        .get("no_agent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    spec.deliver = strings(job.get("deliver"));
    spec.failure_deliver = strings(job.get("failure_deliver"));
    let toolsets = strings(job.get("enabled_toolsets"));
    spec.toolsets = (!toolsets.is_empty()).then_some(toolsets);
    spec.model = text(job, "model").map(str::to_owned);
    spec.provider = text(job, "provider").map(str::to_owned);
    spec.reasoning_effort = text(job, "reasoning_effort").map(str::to_owned);
    spec.timezone = text(job, "timezone").map(str::to_owned);
    spec.workdir = text(job, "workdir").map(str::to_owned);
    spec.interpreter = text(job, "interpreter").map(str::to_owned);
    spec.monitor_url = text(job, "monitor_url").map(str::to_owned);
    spec.context_from = strings(job.get("context_from"));
    spec.grace_secs = job
        .get("grace_minutes")
        .and_then(Value::as_u64)
        .map(|m| m * 60);
    spec.repeat_times = job
        .get("repeat")
        .and_then(|r| r.get("times"))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    let mut script = None;
    if let Some(rel) = text(job, "script") {
        let from = src.profile.path("scripts").join(rel);
        if from.is_file() {
            spec.script = Some(rel.to_owned());
            script = Some(from);
        } else {
            step.entries.push(
                entry(Disposition::Failed).because(format!("its script scripts/{rel} is missing")),
            );
            return;
        }
    }
    let enabled = job.get("enabled").and_then(Value::as_bool).unwrap_or(true)
        && text(job, "state") != Some("paused");
    let state = if step.enable && enabled {
        "enabled"
    } else {
        "paused"
    };
    let (disposition, reason) = match note {
        Some(note) => (Disposition::Transformed, format!("{note}; {state}")),
        None => (Disposition::Mapped, state.to_owned()),
    };
    step.jobs.push(Job {
        name,
        spec,
        enabled,
        script,
        entry: step.entries.len(),
    });
    step.entries.push(
        entry(disposition)
            .to(format!("cron job {item}"))
            .because(reason),
    );
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            s.split(',').map(|p| p.trim().to_owned()).collect()
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// A job's schedule in the Prometheus grammar with a note when it was
/// rewritten; `Ok(None)` for a one-shot that already ran.
fn schedule_of(job: &Value) -> Result<Option<(Schedule, Option<String>)>, String> {
    let completed = job
        .get("repeat")
        .and_then(|r| r.get("completed"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let done = text(job, "state") == Some("completed") || completed > 0;
    let display = text(job, "schedule_display");
    match job.get("schedule") {
        Some(Value::Object(parsed)) => {
            let kind = parsed.get("kind").and_then(Value::as_str).unwrap_or("");
            match kind {
                "interval" => {
                    let minutes = parsed.get("minutes").and_then(Value::as_u64).unwrap_or(0);
                    interval(minutes * 60, display)
                }
                "cron" => {
                    let expr = parsed.get("expr").and_then(Value::as_str).unwrap_or("");
                    cron(expr)
                }
                "once" if done => Ok(None),
                "once" => {
                    let at = parsed.get("run_at").and_then(Value::as_str).unwrap_or("");
                    once(at)
                }
                other => Err(format!("unknown Hermes schedule kind '{other}'")),
            }
        }
        Some(Value::String(raw)) => from_text(raw, done),
        _ => Err("no schedule".to_owned()),
    }
}

fn interval(
    secs: u64,
    display: Option<&str>,
) -> Result<Option<(Schedule, Option<String>)>, String> {
    if secs < pmt_cron::schedule::MIN_SECS {
        return Err(format!(
            "interval of {secs}s is below the Prometheus minimum of 60s"
        ));
    }
    let every = format!("every {}m", secs / 60);
    let bare = display.is_some_and(|d| pmt_cron::duration::parse(&d.to_lowercase()).is_some());
    let note = bare.then(|| {
        format!(
            "bare duration '{}' (recurring in Hermes) rewritten to '{every}'",
            display.unwrap_or_default()
        )
    });
    Ok(Some((Schedule::Interval { every_secs: secs }, note)))
}

fn cron(expr: &str) -> Result<Option<(Schedule, Option<String>)>, String> {
    let utc = jiff::tz::TimeZone::UTC;
    Schedule::parse(expr, Timestamp::now(), &utc)
        .map(|s| Some((s, None)))
        .map_err(|e| e.message().to_owned())
}

fn once(at: &str) -> Result<Option<(Schedule, Option<String>)>, String> {
    let ts = at
        .parse::<Timestamp>()
        .or_else(|_| {
            jiff::civil::DateTime::strptime("%Y-%m-%dT%H:%M:%S%.f", at)
                .or_else(|_| jiff::civil::DateTime::strptime("%Y-%m-%dT%H:%M", at))
                .and_then(|d| d.to_zoned(jiff::tz::TimeZone::system()))
                .map(|z| z.timestamp())
        })
        .map_err(|_| format!("invalid one-shot time '{at}'"))?;
    Ok(Some((
        Schedule::Once {
            at_ms: ts.as_millisecond(),
        },
        None,
    )))
}

/// A schedule kept as Hermes text: Hermes' own reading of it (a bare
/// duration recurs, `in <duration>` and timestamps fire once, anything
/// else is a cron expression or day/time phrase).
fn from_text(raw: &str, done: bool) -> Result<Option<(Schedule, Option<String>)>, String> {
    let raw = raw.trim();
    let lower = raw.to_lowercase();
    if let Some(secs) = pmt_cron::duration::parse(&lower) {
        return interval(secs, Some(raw));
    }
    if let Some(secs) = lower
        .strip_prefix("every ")
        .and_then(pmt_cron::duration::parse)
    {
        return interval(secs, None);
    }
    let timestamp = raw
        .get(..4)
        .is_some_and(|y| y.bytes().all(|b| b.is_ascii_digit()))
        && raw.get(4..5) == Some("-");
    if (timestamp || lower.starts_with("in ")) && done {
        return Ok(None);
    }
    if timestamp {
        return once(raw);
    }
    cron(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_hermes_duration_stays_recurring() {
        let job = serde_json::json!({
            "schedule": {"kind": "interval", "minutes": 30, "display": "every 30m"},
            "schedule_display": "30m",
        });
        let (schedule, note) = schedule_of(&job).unwrap().unwrap();
        assert_eq!(schedule, Schedule::Interval { every_secs: 1800 });
        assert!(note.unwrap().contains("rewritten to 'every 30m'"));
        let (from_text, _) = from_text("30m", false).unwrap().unwrap();
        assert_eq!(from_text, Schedule::Interval { every_secs: 1800 });
    }

    #[test]
    fn completed_one_shots_are_skipped_and_crons_kept() {
        let done = serde_json::json!({
            "schedule": {"kind": "once", "run_at": "2026-01-01T00:00:00+00:00"},
            "repeat": {"times": 1, "completed": 1},
        });
        assert!(schedule_of(&done).unwrap().is_none());
        let (cron, _) =
            schedule_of(&serde_json::json!({"schedule": {"kind": "cron", "expr": "0 9 * * 1-5"}}))
                .unwrap()
                .unwrap();
        assert_eq!(
            cron,
            Schedule::Cron {
                expr: "0 9 * * 1-5".to_owned()
            }
        );
    }
}
