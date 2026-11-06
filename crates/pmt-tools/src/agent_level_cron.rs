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
                "job": {"type": "string", "description": "The job's id or unique name (all actions but create and list)."},
                "name": {"type": "string", "description": "Display name (default: the prompt's start)."},
                "schedule": {"type": "string", "description": "When it runs, in the schedule grammar."},
                "schedule_text": {"type": "string", "description": "A free-form schedule the grammar does not accept, translated by the auxiliary model (needs confirmation)."},
                "prompt": {"type": "string", "description": "A self-contained prompt: the run has no conversation history."},
                "skills": {"type": "array", "items": {"type": "string"}, "description": "Skills preloaded into the run."},
                "script": {"type": "string", "description": "A script in the profile's scripts/ directory, relative to it."},
                "no_agent": {"type": "boolean", "description": "The script is the whole job: its output is delivered as is."},
                "deliver": {"type": "array", "items": {"type": "string"}, "description": "local, origin (this chat), none, <platform> (its home channel), <platform>:<chat_id>[:<thread_id>] or bot_chat[:<profile>]. Default: origin from a chat, else local."},
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
                "model": {"type": "string"},
                "provider": {"type": "string"},
                "reason": {"type": "string", "description": "Why it is paused (pause)."},
                "status": {"type": "string", "description": "Only runs with this status (runs)."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Most runs listed (runs, default 10)."},
                "confirm_token": {"type": "string", "description": "The token of a needs_confirmation answer, after the user agreed."}
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
        ] {
            assert_eq!(
                schema.matches(&format!("\"{field}\":")).count(),
                1,
                "{field}"
            );
        }
    }
}
