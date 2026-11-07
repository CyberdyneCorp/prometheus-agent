//! The stages of a run before anything is recorded: the monitor, the
//! script and its wake gate, the prompt and the agent.

use std::time::Duration;

use serde_json::json;
use sha2::{Digest as _, Sha256};

use super::{
    AgentEnd, AgentOutcome, AgentRequest, Exec, MAX_SCRIPT_STDOUT, Produced, STDERR_TAIL, head,
    tail,
};
use crate::job::{Job, Status};
use crate::prompt::{self, Parts, Wake};
use crate::{deliver, failure, scan, validate};
use pmt_store::cron::RunRow;

/// The skip reason of a script that kept the agent asleep.
pub const WAKE_AGENT_FALSE: &str = "wake_agent_false";
/// The skip reason of an unchanged monitor.
pub const NO_CHANGE: &str = "no_change";

/// Runs the monitor, script and agent stages.
pub(super) async fn produce(ctx: &Exec<'_>, job: &Job, run: &RunRow) -> Produced {
    let mut parts = Parts::default();
    let mut monitored = None;
    if job.spec.monitor_script.is_some() || job.spec.monitor_url.is_some() {
        match monitor(ctx, job).await {
            Ok(Some((section, state))) => {
                parts.monitor = Some(section);
                monitored = Some(state);
            }
            Ok(None) => return Produced::silent(NO_CHANGE),
            Err(produced) => return produced,
        }
    }
    let mut produced = stages_after_monitor(ctx, job, run, parts).await;
    if produced.monitor.is_none() {
        produced.monitor = monitored;
    }
    produced
}

async fn stages_after_monitor(
    ctx: &Exec<'_>,
    job: &Job,
    run: &RunRow,
    mut parts: Parts,
) -> Produced {
    if let Some(script) = &job.spec.script {
        let stdout = match run_script(ctx, job, script).await {
            Ok(stdout) => stdout,
            Err(produced) => return produced,
        };
        match prompt::wake(&stdout) {
            Wake::Skip => return Produced::silent(WAKE_AGENT_FALSE),
            Wake::Run { context, stdout } => {
                parts.wake_context = context;
                parts.script_output = Some(stdout);
            }
        }
    }
    if job.spec.no_agent {
        return no_agent(job, parts.script_output.unwrap_or_default());
    }
    match prompt_parts(ctx, job, run, parts) {
        Ok(parts) => agent(ctx, job, run, &parts).await,
        Err(err) => Produced::failed(Status::Failed, err.message().to_owned(), false),
    }
}

/// The script is the whole job: its output is delivered verbatim.
fn no_agent(job: &Job, stdout: String) -> Produced {
    let empty = stdout.trim().is_empty();
    Produced {
        output: Some(stdout.clone()),
        deliver: (!empty).then_some(stdout),
        inputs: json!({"script": job.spec.script}),
        ..Produced::ended(if empty { Status::Silent } else { Status::Ok }, true)
    }
}

/// The notepad, upstream outputs and run context of the prompt.
fn prompt_parts(
    ctx: &Exec<'_>,
    job: &Job,
    run: &RunRow,
    mut parts: Parts,
) -> Result<Parts, pmt_core::error::Error> {
    parts.notepad = crate::notepad::section(&ctx.store.cron_notepad(job.id())?);
    parts.upstream = prompt::upstream(ctx.store, job, &run.id)?;
    parts.run_context.clone_from(&run.run_context);
    Ok(parts)
}

async fn agent(ctx: &Exec<'_>, job: &Job, run: &RunRow, parts: &Parts) -> Produced {
    let prompt = prompt::assemble(&job.spec.prompt, parts);
    let scanned = format!("{prompt}\n{}", ctx.runner.skill_text(job));
    if let Some(rule) = scan::violation(&scanned) {
        return Produced::failed(Status::Failed, format!("unsafe prompt: {rule}"), true);
    }
    let inputs = json!({"prompt": prompt, "skills": job.spec.skills,
        "script_output": parts.script_output, "run_context": parts.run_context});
    let (idle, wall) = ctx.limits(job);
    let request = AgentRequest {
        job: job.clone(),
        run_id: run.id.clone(),
        prompt,
        idle,
        wall,
        replay: false,
    };
    let outcome = ctx.runner.run_agent(request).await;
    agent_result(outcome, inputs)
}

/// How an agent run maps to a run's result.
pub(super) fn agent_result(outcome: AgentOutcome, inputs: serde_json::Value) -> Produced {
    let mut produced = match &outcome.end {
        AgentEnd::Completed(text) if deliver::is_silent(text) => Produced {
            output: Some(text.clone()),
            ..Produced::ended(Status::Silent, true)
        },
        AgentEnd::Completed(text) => match failure::declared(text) {
            Some(error) => Produced {
                output: Some(text.clone()),
                ..Produced::failed(Status::Failed, error, true)
            },
            None => Produced {
                output: Some(text.clone()),
                deliver: Some(text.clone()),
                ..Produced::ended(Status::Ok, true)
            },
        },
        AgentEnd::Failed(error) => Produced::failed(Status::Failed, error.clone(), true),
        AgentEnd::Unreachable(error) => Produced {
            unreachable: true,
            ..Produced::failed(Status::Failed, error.clone(), true)
        },
        AgentEnd::UsageClosed(until) => {
            let reason = format!(
                "usage window closed until {}",
                crate::schedule::at_ms(*until)
            );
            Produced {
                reason: Some(reason.clone()),
                hold_until: Some(*until),
                ..Produced::failed(Status::Failed, reason, true)
            }
        }
        AgentEnd::TimedOut(reason) => Produced {
            reason: Some((*reason).to_owned()),
            ..Produced::failed(
                Status::TimedOut,
                format!("interrupted after the {reason} timeout"),
                true,
            )
        },
    };
    produced.reached_model = outcome.reached_model;
    produced.inputs = inputs;
    produced.agent = Some(outcome);
    produced
}

async fn run_script(ctx: &Exec<'_>, job: &Job, script: &str) -> Result<String, Produced> {
    let path = validate::script_path(ctx.scripts_dir, script)
        .map_err(|e| Produced::failed(Status::Failed, e.message().to_owned(), false))?;
    let timeout = Duration::from_secs(ctx.config.script_timeout_secs.max(1));
    let outcome = ctx.runner.run_script(job, &path, timeout).await;
    if outcome.timed_out {
        return Err(Produced {
            reason: Some("script".to_owned()),
            ..Produced::failed(
                Status::TimedOut,
                format!("script ran past {} s", timeout.as_secs()),
                true,
            )
        });
    }
    if outcome.exit_code != Some(0) {
        let code = outcome
            .exit_code
            .map_or_else(|| "a signal".to_owned(), |c| format!("exit {c}"));
        let stderr = tail(&outcome.stderr, STDERR_TAIL);
        return Err(Produced::failed(
            Status::Failed,
            format!("script failed ({code}): {}", stderr.trim_end()),
            true,
        ));
    }
    Ok(head(&outcome.stdout, MAX_SCRIPT_STDOUT))
}

/// The SHA-256 of `text`, hex.
fn sha256(text: &str) -> String {
    crate::job::hex(&Sha256::digest(text.as_bytes()))
}

/// Runs the monitor: `None` when its output did not change since the
/// previous tick, else the change section and the new (hash, output).
async fn monitor(
    ctx: &Exec<'_>,
    job: &Job,
) -> Result<Option<(String, (String, String))>, Produced> {
    let output =
        match (&job.spec.monitor_script, &job.spec.monitor_url) {
            (Some(script), _) => run_script(ctx, job, script).await?,
            (None, Some(url)) => ctx.runner.fetch(url).await.map_err(|e| {
                Produced::failed(Status::Failed, format!("monitor failed: {e}"), true)
            })?,
            (None, None) => String::new(),
        };
    let hash = sha256(&output);
    if job.row.monitor_hash.as_deref() == Some(hash.as_str()) {
        return Ok(None);
    }
    let section = prompt::monitor_section(job.row.monitor_output.as_deref(), &output);
    Ok(Some((section, (hash, output))))
}
