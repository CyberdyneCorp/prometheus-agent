//! The `goal_judge` contract (spec: session-automation, Goal judge loop;
//! Completion contracts; Subgoals; Loop stop conditions): the prompts the
//! auxiliary task is asked and the one-line JSON verdict it answers. A
//! reply that does not parse is an error the caller treats as `continue`
//! (the judge fails open).

use std::fmt::Write as _;

use serde::Deserialize;

use crate::contract::Contract;
use crate::state::Goal;

/// Characters of the final assistant response the judge sees.
pub const RESPONSE_TAIL_CHARS: usize = 4_000;

/// The heading the continuation prompt lists subgoals under.
pub const SUBGOALS_HEADING: &str = "Additional criteria the user added mid-loop";

/// What a goal waits for on a `wait` verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOn {
    /// A background process of the session (`proc_…`).
    Session(String),
    /// A local pid.
    Pid(u32),
    /// A number of seconds.
    Seconds(u64),
}

/// A verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The goal (and every subgoal) is met.
    Done(String),
    /// Another turn is needed.
    Continue(String),
    /// It cannot be achieved as things stand: pause, never done.
    Blocked(String),
    /// Park until something happens.
    Wait {
        /// On what.
        on: WaitOn,
        /// Why.
        reason: String,
    },
}

impl Verdict {
    /// The judge's reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Done(r) | Self::Continue(r) | Self::Blocked(r) | Self::Wait { reason: r, .. } => {
                r
            }
        }
    }
}

/// A background process the judge may tell the goal to wait on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    /// Its process id (`proc_…`).
    pub id: String,
    /// Its OS pid, when local.
    pub pid: Option<u32>,
    /// Its command line.
    pub command: String,
}

/// The last [`RESPONSE_TAIL_CHARS`] characters of `response`.
#[must_use]
pub fn tail(response: &str) -> &str {
    let count = response.chars().count();
    if count <= RESPONSE_TAIL_CHARS {
        return response;
    }
    let skip = count - RESPONSE_TAIL_CHARS;
    let at = response
        .char_indices()
        .nth(skip)
        .map_or(response.len(), |(i, _)| i);
    &response[at..]
}

/// The goal, its contract and subgoals as the judge and the continuation
/// prompt state them.
fn goal_block(goal: &Goal) -> String {
    let mut text = format!("Goal: {}", goal.text);
    if let Some(contract) = goal.contract.as_ref().filter(|c| !c.is_empty()) {
        let _ = write!(text, "\n\nCompletion contract:\n{}", contract.render());
    }
    if !goal.subgoals.is_empty() {
        let _ = write!(text, "\n\n{SUBGOALS_HEADING}:");
        for (n, subgoal) in goal.subgoals.iter().enumerate() {
            let _ = write!(text, "\n{}. {subgoal}", n + 1);
        }
    }
    text
}

/// The judge prompt after a turn of `goal` that ended with `response`
/// while the session's own background `processes` run.
#[must_use]
pub fn prompt(goal: &Goal, response: &str, processes: &[Process]) -> String {
    let mut text = String::from(
        "You are the goal judge of an agent session. Decide whether the goal below is achieved, \
         based only on the agent's latest response.\n\n",
    );
    text.push_str(&goal_block(goal));
    if goal
        .contract
        .as_ref()
        .is_some_and(|c| c.verification.is_some())
    {
        text.push_str(
            "\n\nAnswer done only when the response shows concrete evidence for the \
             verification above.",
        );
    }
    if !goal.subgoals.is_empty() {
        text.push_str("\n\nAnswer done only when the goal and every criterion above are met.");
    }
    text.push_str("\n\nBackground processes of the session:");
    if processes.is_empty() {
        text.push_str(" none");
    }
    for process in processes {
        let pid = process.pid.map_or(String::new(), |p| format!(" pid {p}"));
        let _ = write!(text, "\n- {}{pid}: {}", process.id, process.command);
    }
    let _ = write!(
        text,
        "\n\nLatest agent response (last {RESPONSE_TAIL_CHARS} characters):\n<response>\n{}\n</response>\n\n",
        tail(response)
    );
    text.push_str(VERDICT_FORMAT);
    text
}

const VERDICT_FORMAT: &str = "Reply with exactly one line of JSON: {\"verdict\": \"done\"|\"continue\"|\"blocked\"|\"wait\", \"reason\": \"...\"}. Use \"blocked\" when the goal cannot be achieved as things stand. For \"wait\" add one of \"wait_on_session\" (a background process id above), \"wait_on_pid\" or \"wait_for_seconds\".";

/// The user turn a continuation injects: the goal (with its contract and
/// subgoals) and the judge's reason.
#[must_use]
pub fn continuation(goal: &Goal, reason: &str) -> String {
    let mut text = format!(
        "[goal continuation] Keep working toward the goal.\n\n{}",
        goal_block(goal)
    );
    if !reason.is_empty() {
        let _ = write!(text, "\n\nWhat remains: {reason}");
    }
    text
}

/// The continuation after a red quality gate.
#[must_use]
pub fn gate_continuation(
    goal: &Goal,
    gate: usize,
    command: &str,
    exit: i32,
    output: &str,
) -> String {
    format!(
        "[goal continuation] Quality gate {gate} (`{command}`) failed with exit {exit}. Fix it, \
         then keep working toward the goal.\n\nOutput (tail):\n{output}\n\n{}",
        goal_block(goal)
    )
}

/// The `/goal draft` prompt.
#[must_use]
pub fn draft_prompt(objective: &str) -> String {
    format!(
        "You are the goal judge of an agent session. Expand the objective below into a \
         completion contract. Reply with exactly one line of JSON: {{\"outcome\": \"...\", \
         \"verification\": \"...\", \"constraints\": \"...\", \"boundaries\": \"...\", \
         \"stop_when\": \"...\"}} (omit a field you cannot state).\n\nObjective: {objective}"
    )
}

/// The `--until` prompt after a loop wakeup.
#[must_use]
pub fn until_prompt(condition: &str, response: &str) -> String {
    format!(
        "You are the goal judge of an agent session. A loop runs until a condition holds. \
         Decide from the latest response whether it holds now.\n\nCondition: {condition}\n\n\
         Latest agent response:\n<response>\n{}\n</response>\n\nReply with exactly one line of \
         JSON: {{\"verdict\": \"done\"|\"continue\"|\"blocked\", \"reason\": \"...\"}}: done when \
         the condition holds, blocked when it can never hold.",
        tail(response)
    )
}

#[derive(Debug, Deserialize)]
struct Raw {
    verdict: Option<String>,
    done: Option<bool>,
    #[serde(default)]
    reason: String,
    wait_on_session: Option<String>,
    wait_on_pid: Option<u32>,
    wait_for_seconds: Option<u64>,
}

/// The JSON objects in `text`: each line, then the whole text (code
/// fences and prose around the line are tolerated).
fn objects<'de, T: Deserialize<'de>>(text: &'de str) -> Option<T> {
    text.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .find_map(|l| serde_json::from_str(l).ok())
        .or_else(|| {
            let start = text.find('{')?;
            let end = text.rfind('}')?;
            serde_json::from_str(text.get(start..=end)?).ok()
        })
}

/// Parses a judge reply.
///
/// # Errors
///
/// What is wrong with a reply that is no verdict.
pub fn parse(text: &str) -> Result<Verdict, String> {
    let raw: Raw = objects(text).ok_or_else(|| "the judge reply is not JSON".to_owned())?;
    let reason = raw.reason.trim().to_owned();
    match (raw.verdict.as_deref(), raw.done) {
        (Some("done"), _) | (None, Some(true)) => Ok(Verdict::Done(reason)),
        (Some("continue"), _) | (None, Some(false)) => Ok(Verdict::Continue(reason)),
        (Some("blocked" | "unachievable"), _) => Ok(Verdict::Blocked(reason)),
        (Some("wait"), _) => wait(raw, reason),
        (Some(other), _) => Err(format!("unknown verdict '{other}'")),
        (None, None) => Err("the judge reply has no verdict".to_owned()),
    }
}

fn wait(raw: Raw, reason: String) -> Result<Verdict, String> {
    let on = if let Some(id) = raw.wait_on_session {
        WaitOn::Session(id)
    } else if let Some(pid) = raw.wait_on_pid {
        WaitOn::Pid(pid)
    } else if let Some(secs) = raw.wait_for_seconds {
        WaitOn::Seconds(secs)
    } else {
        return Err("a wait verdict names nothing to wait on".to_owned());
    };
    Ok(Verdict::Wait { on, reason })
}

/// Parses a `/goal draft` reply into a contract.
///
/// # Errors
///
/// A reply that is not a contract with at least one field.
pub fn parse_draft(text: &str) -> Result<Contract, String> {
    let contract: Contract =
        objects(text).ok_or_else(|| "the draft reply is not JSON".to_owned())?;
    if contract.is_empty() {
        return Err("the draft reply has no contract field".to_owned());
    }
    Ok(contract)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_parse_with_legacy_and_fences() {
        assert_eq!(
            parse("{\"verdict\":\"continue\",\"reason\":\"3 files remain\"}"),
            Ok(Verdict::Continue("3 files remain".into()))
        );
        assert_eq!(
            parse("```json\n{\"done\": true, \"reason\": \"ok\"}\n```"),
            Ok(Verdict::Done("ok".into()))
        );
        assert_eq!(
            parse("{\"verdict\":\"wait\",\"wait_on_pid\":4242,\"reason\":\"CI running\"}"),
            Ok(Verdict::Wait {
                on: WaitOn::Pid(4242),
                reason: "CI running".into()
            })
        );
        assert!(parse("I think it is done").is_err());
        assert!(parse("{\"verdict\":\"maybe\"}").is_err());
    }

    #[test]
    fn the_judge_prompt_states_subgoals_and_the_response_tail() {
        let mut goal = Goal::new("fix the failing tests".into(), None);
        goal.subgoals.push("add a regression test".into());
        let response = format!("{}END", "x".repeat(5_000));
        let prompt = prompt(&goal, &response, &[]);
        assert!(prompt.contains(&format!("{SUBGOALS_HEADING}:\n1. add a regression test")));
        assert!(prompt.contains("every criterion above"));
        assert!(prompt.contains("END\n</response>"));
        assert!(!prompt.contains(&"x".repeat(4_001)));
    }

    #[test]
    fn drafts_parse_into_contracts() {
        let contract =
            parse_draft("{\"outcome\":\"JWT auth\",\"verification\":\"tests pass\"}").unwrap();
        assert_eq!(contract.outcome.as_deref(), Some("JWT auth"));
        assert!(parse_draft("{}").is_err());
    }
}
