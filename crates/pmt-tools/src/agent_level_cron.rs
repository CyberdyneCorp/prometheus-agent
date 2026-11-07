//! The registry entry of `cronjob` (spec: scheduled-jobs, Model tool
//! surface): the turn loop runs it through the session's cron port. It is
//! interactive (its create and schedule edits need the user's agreement
//! through a confirmation round-trip), so sessions nobody can answer
//! (scheduled runs, one-shot runs, the API) do not get it.

use serde_json::json;

use crate::descriptor::{Effect, ResourceClaims, ToolDescriptor};

/// The tool's name.
pub const NAME: &str = "cronjob";

/// `cronjob`.
#[must_use]
pub fn cronjob() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        NAME,
        "cronjob",
        "Create and manage scheduled jobs that run a prompt, skills or a script later and \
         deliver the result to this chat, a home channel, a file or nowhere. Schedules: \
         'every 30m', 'in 2h', 'tomorrow at 9am', 'weekdays at 9:30', a 5-field cron \
         expression or an RFC 3339 time (a bare '30m' is refused). create and schedule \
         changes return status needs_confirmation with the schedule echo and a \
         confirm_token: show the echo to the user and repeat the same call with the token \
         only after they agree. A job is named by its id or unique name.",
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "list", "update", "pause", "resume", "remove", "run", "runs"]},
                "job_id": {"type": "string", "description": "The job's id or unique name (all actions but create and list)."},
                "confirm_token": {"type": "string", "description": "The token of a needs_confirmation answer, after the user agreed."},
                "name": {"type": "string", "description": "Display name (default: the prompt's start)."},
                "schedule": {"type": "string", "description": "When it runs, in the schedule grammar."},
                "schedule_text": {"type": "string", "description": "A free-form schedule the grammar does not accept, translated by the auxiliary model (needs confirmation)."},
                "prompt": {"type": "string", "description": "A self-contained prompt: the run has no conversation history. With action run: context appended to that run only (## Run Context)."},
                "skills": {"type": "array", "items": {"type": "string"}, "description": "Skills preloaded into the run."},
                "script": {"type": "string", "description": "A script file in the profile's scripts/ directory, relative to it (never a command line). Its last stdout line {\"wakeAgent\": false} skips the run."},
                "interpreter": {"type": "string", "description": "The absolute interpreter of a .py script (default: the managed Python)."},
                "script_secrets": {"type": "array", "items": {"type": "string"}, "description": "Secrets passed to the script's environment (never provider or gateway credentials)."},
                "no_agent": {"type": "boolean", "description": "The script is the whole job: its output is delivered as is."},
                "monitor_script": {"type": "string", "description": "A script in scripts/ whose output is watched: the agent runs only when it changes."},
                "monitor_url": {"type": "string", "description": "An http(s) URL watched the same way."},
                "deliver": {"type": "array", "items": {"type": "string"}, "description": "local, origin (this chat), none, <platform> (its home channel), <platform>:<chat_id>[:<thread_id>] or bot_chat[:<profile>]. Default: origin from a chat, else local."},
                "failure_deliver": {"type": "array", "items": {"type": "string"}, "description": "Where failure notices go instead of deliver."},
                "attach_to_session": {"type": "boolean", "description": "A reply to a delivery continues the conversation."},
                "toolsets": {"type": "array", "items": {"type": "string"}, "description": "Toolsets of the run ([] = no tools; default cron.default_toolsets)."},
                "memory": {"type": "string", "enum": ["read_only", "read_write", "off"], "description": "Default read_only."},
                "grace": {"type": "string", "description": "How late a run may start, 0s to 24h (default: half the period, 2 min to 2 h)."},
                "timeout": {
                    "type": "object",
                    "properties": {
                        "idle": {"type": "string", "description": "Interrupt after this long without activity (0s = unlimited)."},
                        "wall": {"type": "string", "description": "Interrupt after this long in all (0s = unlimited)."}
                    },
                    "additionalProperties": false
                },
                "repeat": {"type": "integer", "minimum": 1, "description": "Runs before the job completes (default: unlimited; 1 for a one-shot)."},
                "timezone": {"type": "string", "description": "The IANA zone the schedule runs in (empty clears)."},
                "workdir": {"type": "string", "description": "An existing absolute directory the run works in, loading its project context files (empty clears)."},
                "context_from": {"type": ["string", "array"], "items": {"type": "string"}, "description": "Jobs (ids or names) whose latest output is prepended to the prompt."},
                "continuity": {"type": "boolean", "description": "Prepend this job's own previous report."},
                "pinned": {"type": "boolean", "description": "Only when the user asks: true freezes the current main model onto the job, false follows the cron default again."},
                "paused": {"type": "boolean", "description": "Create (or set) the job paused: nothing fires until resumed."},
                "paused_reason": {"type": "string", "description": "Why it is paused."},
                "status": {"type": "string", "description": "Only runs with this status (runs)."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Most runs listed (runs, default 10)."}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    );
    d.effect = Effect::External;
    d.claims = Some(ResourceClaims {
        interactive: true,
        ..ResourceClaims::exclusive(NAME)
    });
    d
}

#[cfg(test)]
mod tests {
    use super::cronjob;

    /// Scenario field set: every accepted field is in the schema once.
    #[test]
    fn the_schema_lists_every_field_once() {
        let d = cronjob();
        let schema = serde_json::to_string(&d.parameters).unwrap();
        for field in [
            "grace",
            "timeout",
            "memory",
            "toolsets",
            "schedule_text",
            "confirm_token",
            "timezone",
            "workdir",
            "pinned",
            "paused_reason",
        ] {
            assert_eq!(
                schema.matches(&format!("\"{field}\":")).count(),
                1,
                "{field}"
            );
        }
        // The agent never chooses a model.
        let properties = d.parameters["properties"].as_object().unwrap();
        for field in ["model", "provider", "reasoning_effort"] {
            assert!(!properties.contains_key(field), "{field}");
        }
    }

    /// Scenario: Agent cannot choose a model.
    #[test]
    fn a_model_argument_fails_schema_validation() {
        let d = cronjob();
        let violations = crate::args::validate(
            &d.parameters,
            &serde_json::json!({"action": "update", "job_id": "x", "model": "x"}),
        );
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].to_string(), "/model: unknown field 'model'");
    }
}
