//! A run's prompt (spec: scheduled-jobs, Per-job notepad; Job chaining and
//! continuity; Script wake gate; Monitor mode; Asynchronous tool-initiated
//! runs with per-run context): the stored prompt with the sections the
//! run adds, in this order: the job notepad, upstream outputs (in
//! `context_from` order), the prompt, the script's wake context and
//! output, the monitor change, the run context.

use pmt_core::error::Error;
use pmt_store::Store;
use pmt_store::cron::RunRow;
use serde_json::Value;

use crate::job::{Job, SELF, Status};
use crate::ops;

/// An upstream output injected at most.
pub const MAX_UPSTREAM: usize = 64 * 1024;
/// A monitor diff injected at most.
pub const MAX_MONITOR_DIFF: usize = 16 * 1024;

/// What a script's last stdout line says about waking the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Wake {
    /// `{"wakeAgent": false}`: skip the agent (or delivery).
    Skip,
    /// Run, with an optional context object; `stdout` without the gate
    /// line.
    Run {
        /// The `context` object.
        context: Option<Value>,
        /// The output without the gate line.
        stdout: String,
    },
}

/// Reads the wake gate from a script's output: an absent or unparseable
/// last line means wake (with the whole output).
#[must_use]
pub fn wake(stdout: &str) -> Wake {
    let trimmed = stdout.trim_end();
    let (head, last) = trimmed.rsplit_once('\n').unwrap_or(("", trimmed));
    let gate = serde_json::from_str::<Value>(last.trim())
        .ok()
        .filter(|v| v.get("wakeAgent").is_some_and(Value::is_boolean));
    match gate {
        Some(gate) if gate["wakeAgent"] == Value::Bool(false) => Wake::Skip,
        Some(gate) => Wake::Run {
            context: gate.get("context").filter(|c| !c.is_null()).cloned(),
            stdout: head.to_owned(),
        },
        None => Wake::Run {
            context: None,
            stdout: stdout.to_owned(),
        },
    }
}

/// The run's parts besides the stored prompt.
#[derive(Debug, Clone, Default)]
pub struct Parts {
    /// The `## Job notepad` section.
    pub notepad: Option<String>,
    /// Upstream sections.
    pub upstream: Vec<String>,
    /// The script's wake context.
    pub wake_context: Option<Value>,
    /// The script's output.
    pub script_output: Option<String>,
    /// The monitor change section.
    pub monitor: Option<String>,
    /// The transient run context.
    pub run_context: Option<String>,
}

fn cut(text: &str, max: usize) -> String {
    crate::incident::truncate(text, max)
}

/// The assembled prompt.
#[must_use]
pub fn assemble(prompt: &str, parts: &Parts) -> String {
    let script = parts
        .script_output
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    if parts.notepad.is_none()
        && parts.upstream.is_empty()
        && parts.wake_context.is_none()
        && !script
        && parts.monitor.is_none()
        && parts.run_context.is_none()
    {
        return prompt.to_owned();
    }
    let mut sections: Vec<String> = Vec::new();
    sections.extend(parts.notepad.clone());
    sections.extend(parts.upstream.iter().cloned());
    sections.push(prompt.trim_end().to_owned());
    if let Some(context) = &parts.wake_context {
        let json = serde_json::to_string_pretty(context).unwrap_or_default();
        sections.push(format!("## Script context\n\n```json\n{json}\n```"));
    }
    if let Some(stdout) = parts.script_output.as_deref().filter(|s| !s.is_empty()) {
        sections.push(format!(
            "## Script output\n\n{}",
            cut(stdout, crate::exec::MAX_SCRIPT_STDOUT)
        ));
    }
    sections.extend(parts.monitor.clone());
    if let Some(context) = &parts.run_context {
        sections.push(format!("## Run Context\n\n{}", context.trim()));
    }
    sections.retain(|s| !s.is_empty());
    sections.join("\n\n")
}

/// The `MONITOR CHANGE DETECTED` section between two outputs.
#[must_use]
pub fn monitor_section(previous: Option<&str>, current: &str) -> String {
    let diff = crate::diff::unified(previous.unwrap_or(""), current, "previous", "current");
    format!(
        "## MONITOR CHANGE DETECTED\n\nThe monitored output changed since the last check:\n\n\
         ```diff\n{}\n```",
        cut(&diff, MAX_MONITOR_DIFF).trim_end()
    )
}

/// A run whose output an upstream section may carry: completed, not
/// silent.
fn carries_output(run: &RunRow) -> bool {
    let status = Status::parse(&run.status);
    matches!(
        status,
        Some(Status::Ok | Status::Failed | Status::DeliveryQueued | Status::DeliveryFailed)
    ) && run.output.as_deref().is_some_and(|o| !o.trim().is_empty())
}

/// The newest output-carrying run of `job_id` (other than `current`).
fn latest(store: &Store, job_id: &str, current: &str) -> Result<Option<RunRow>, Error> {
    Ok(store
        .cron_runs(job_id, None, 50)?
        .into_iter()
        .find(|r| r.id != current && carries_output(r)))
}

/// The upstream sections of `job` for run `current` (a missing upstream
/// job or output injects nothing; a running upstream is never waited
/// for).
///
/// # Errors
///
/// Store errors.
pub fn upstream(store: &Store, job: &Job, current: &str) -> Result<Vec<String>, Error> {
    let mut sections = Vec::new();
    for source in &job.spec.context_from {
        let (upstream, own) = if source == SELF {
            (job.clone(), true)
        } else {
            match ops::find(store, source) {
                Ok(found) => (found, false),
                Err(_) => continue,
            }
        };
        let Some(run) = latest(store, upstream.id(), current)? else {
            continue;
        };
        let output = cut(run.output.as_deref().unwrap_or_default(), MAX_UPSTREAM);
        let when = crate::schedule::at_ms(run.started_at.unwrap_or(run.scheduled_at));
        sections.push(if own {
            format!(
                "## Previous run of this job ({when})\n\nYour previous run reported the \
                 following; do not repeat it:\n\n{}",
                output.trim_end()
            )
        } else {
            format!(
                "## Output of job {} ({when})\n\n{}",
                upstream.row.name,
                output.trim_end()
            )
        });
    }
    Ok(sections)
}

#[cfg(test)]
mod tests {
    use super::{Parts, Wake, assemble, wake};

    #[test]
    fn the_wake_gate_reads_the_last_line() {
        assert_eq!(wake("checked\n{\"wakeAgent\": false}\n"), Wake::Skip);
        assert_eq!(
            wake("x\n{\"wakeAgent\": true, \"context\": {\"n\": 2}}"),
            Wake::Run {
                context: Some(serde_json::json!({"n": 2})),
                stdout: "x".into()
            }
        );
        assert_eq!(
            wake("plain output"),
            Wake::Run {
                context: None,
                stdout: "plain output".into()
            }
        );
        assert!(matches!(wake("{not json"), Wake::Run { .. }));
    }

    #[test]
    fn an_empty_notepad_leaves_the_prompt_identical() {
        assert_eq!(assemble("digest", &Parts::default()), "digest");
        let parts = Parts {
            notepad: Some("## Job notepad\n\n- a: 1".into()),
            run_context: Some("focus on EU".into()),
            ..Parts::default()
        };
        let prompt = assemble("digest", &parts);
        assert!(prompt.starts_with("## Job notepad"));
        assert!(prompt.ends_with("## Run Context\n\nfocus on EU"));
    }
}
