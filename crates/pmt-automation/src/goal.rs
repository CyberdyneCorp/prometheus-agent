//! What a turn boundary and the goal commands do to a goal (spec:
//! session-automation, Goal judge loop; Goal turn budget and blocked
//! verdict; Quality gates; Goal wait barriers; Subgoals; Goal command
//! surface). Pure transitions: the host runs the gates and the judge and
//! hands their results here, then persists the goal and acts on the
//! returned [`Step`].

use std::fmt::Write as _;

use crate::command::{GoalCommand, SubgoalCommand};
use crate::judge::{self, Verdict, WaitOn};
use crate::state::{Barrier, Gate, Goal, GoalStatus, Limits};

/// Bytes of a failed gate's combined output the continuation carries.
pub const GATE_OUTPUT_TAIL: usize = 3_072;

/// What the boundary does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Run `prompt` as the next (automated) user turn.
    Continue {
        /// The continuation turn.
        prompt: String,
        /// The status line (`Continuing toward goal (1/20): …`).
        notice: String,
    },
    /// The goal is achieved.
    Done(String),
    /// The goal paused (budget, blocked, exhausted gate).
    Paused(String),
    /// The goal parked on a wait barrier.
    Parked(String),
}

impl Step {
    /// The status line to show.
    #[must_use]
    pub fn notice(&self) -> &str {
        match self {
            Self::Continue { notice, .. }
            | Self::Done(notice)
            | Self::Paused(notice)
            | Self::Parked(notice) => notice,
        }
    }
}

/// How one gate ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRun {
    /// Its exit code (`-1` when it could not run or timed out).
    pub exit: i32,
    /// Combined output.
    pub output: String,
}

/// What the host can tell about processes.
pub trait Probe {
    /// The start time of local pid `pid`, when it runs.
    fn pid_start(&self, pid: u32) -> Option<u64>;
    /// Watch matches of the session's background process `id`, when it
    /// still runs.
    fn process(&self, id: &str) -> Option<u32>;
}

/// The budget pause line.
#[must_use]
pub fn budget_notice(max: u32) -> String {
    format!(
        "Goal paused — {max}/{max} turns used. Use /goal resume to keep going, or /goal clear to stop."
    )
}

/// Counts one continuation turn, or pauses the goal when the budget is
/// spent.
fn continue_turn(goal: &mut Goal, reason: &str, prompt: String, limits: &Limits) -> Step {
    let max = limits.goal_max_turns;
    if goal.turns_used >= max {
        goal.status = GoalStatus::Paused;
        goal.reason = Some(format!("{max}/{max} turns used"));
        return Step::Paused(budget_notice(max));
    }
    goal.turns_used += 1;
    let mut notice = format!("Continuing toward goal ({}/{max})", goal.turns_used);
    if !reason.is_empty() {
        let _ = write!(notice, ": {reason}");
    }
    Step::Continue { prompt, notice }
}

/// The last [`GATE_OUTPUT_TAIL`] bytes of `output` (on a char boundary).
fn output_tail(output: &str) -> &str {
    if output.len() <= GATE_OUTPUT_TAIL {
        return output;
    }
    let mut start = output.len() - GATE_OUTPUT_TAIL;
    while !output.is_char_boundary(start) {
        start += 1;
    }
    &output[start..]
}

/// Records this boundary's gate results; `Some` when a gate is red (the
/// judge is then skipped): a continuation with its exit code and output
/// tail, or a pause once it failed `goals.gate_max_retries` times in a row.
pub fn gates(goal: &mut Goal, runs: &[GateRun], limits: &Limits) -> Option<Step> {
    let mut red = None;
    for (index, (gate, run)) in goal.gates.iter_mut().zip(runs).enumerate() {
        if run.exit == 0 {
            gate.failures = 0;
            continue;
        }
        gate.failures += 1;
        if red.is_none() {
            red = Some((index, gate.clone(), run.clone()));
        }
    }
    let (index, gate, run) = red?;
    let n = index + 1;
    if gate.failures >= limits.gate_max_retries {
        goal.status = GoalStatus::Paused;
        let notice = format!(
            "Goal paused — quality gate {n} (`{}`) failed {} times in a row. Fix it and /goal resume, or /goal gate remove {n}.",
            gate.command, gate.failures
        );
        goal.reason = Some(notice.clone());
        return Some(Step::Paused(notice));
    }
    let tail = output_tail(&run.output);
    let prompt = judge::gate_continuation(goal, n, &gate.command, run.exit, tail);
    let reason = format!("quality gate {n} failed (exit {})", run.exit);
    Some(continue_turn(goal, &reason, prompt, limits))
}

/// Applies the judge's verdict (`Err`: the judge failed, which is
/// `continue`).
pub fn verdict(
    goal: &mut Goal,
    verdict: Result<Verdict, String>,
    limits: &Limits,
    now_ms: i64,
    probe: &dyn Probe,
) -> Step {
    let verdict = verdict.unwrap_or_else(|_| Verdict::Continue(String::new()));
    goal.reason = Some(verdict.reason().to_owned()).filter(|r| !r.is_empty());
    match verdict {
        Verdict::Done(reason) => {
            goal.status = GoalStatus::Done;
            Step::Done(with_reason("Goal achieved", &reason))
        }
        Verdict::Blocked(reason) => {
            goal.status = GoalStatus::Paused;
            Step::Paused(with_reason("Goal judged unachievable — paused", &reason))
        }
        Verdict::Continue(reason) => {
            let prompt = judge::continuation(goal, &reason);
            continue_turn(goal, &reason, prompt, limits)
        }
        Verdict::Wait { on, reason } => wait(goal, on, &reason, limits, now_ms, probe),
    }
}

fn with_reason(head: &str, reason: &str) -> String {
    if reason.is_empty() {
        head.to_owned()
    } else {
        format!("{head}: {reason}")
    }
}

fn wait(
    goal: &mut Goal,
    on: WaitOn,
    reason: &str,
    limits: &Limits,
    now_ms: i64,
    probe: &dyn Probe,
) -> Step {
    let cap = now_ms + secs_ms(limits.max_wait_secs);
    let barrier = match on {
        WaitOn::Pid(pid) => probe.pid_start(pid).map(|start_time| Barrier::Pid {
            pid,
            start_time,
            until_ms: cap,
            reason: reason.to_owned(),
        }),
        WaitOn::Session(id) => probe.process(&id).map(|matches| Barrier::Process {
            id,
            matches,
            until_ms: cap,
            reason: reason.to_owned(),
        }),
        WaitOn::Seconds(secs) => Some(Barrier::Seconds {
            until_ms: (now_ms + secs_ms(secs)).min(cap),
            reason: reason.to_owned(),
        }),
    };
    let Some(barrier) = barrier else {
        // Nothing to wait on: keep going.
        let prompt = judge::continuation(goal, reason);
        return continue_turn(goal, reason, prompt, limits);
    };
    let notice = format!("Goal parked on {}", barrier.describe());
    goal.barrier = Some(barrier);
    Step::Parked(notice)
}

fn secs_ms(secs: u64) -> i64 {
    i64::try_from(secs.saturating_mul(1_000)).unwrap_or(i64::MAX)
}

/// Whether a parked goal's barrier released at `now_ms`; when it did the
/// barrier is dropped and the goal continues (one counted turn).
pub fn release(goal: &mut Goal, limits: &Limits, now_ms: i64, probe: &dyn Probe) -> Option<Step> {
    if goal.status != GoalStatus::Active {
        return None;
    }
    let barrier = goal.barrier.as_ref()?;
    let released = now_ms >= barrier.until_ms()
        || match barrier {
            Barrier::Pid {
                pid, start_time, ..
            } => probe
                .pid_start(*pid)
                .is_none_or(|s| *start_time != 0 && s != *start_time),
            Barrier::Process { id, matches, .. } => probe.process(id).is_none_or(|m| m > *matches),
            Barrier::Seconds { .. } => false,
        };
    if !released {
        return None;
    }
    let what = goal
        .barrier
        .take()
        .map(|b| b.describe())
        .unwrap_or_default();
    let reason = format!("the wait on {what} is over");
    let prompt = judge::continuation(goal, &reason);
    Some(continue_turn(goal, &reason, prompt, limits))
}

/// What a `/goal` or `/subgoal` command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// What to tell the person.
    pub text: String,
    /// The stored goal changed.
    pub changed: bool,
}

impl Outcome {
    fn says(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            changed: false,
        }
    }

    fn changed(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            changed: true,
        }
    }
}

/// The reply of a command that needs a goal when none is set.
pub const NO_GOAL: &str = "No active goal.";

/// When and how a command runs.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    /// The configured bounds.
    pub limits: &'a Limits,
    /// A turn runs now.
    pub running: bool,
    /// The clock (ms since the epoch).
    pub now_ms: i64,
    /// Process liveness.
    pub probe: &'a dyn Probe,
}

/// Applies a `/goal` command to `slot` (`draft` sets the text as is: the
/// host asks the judge for its contract first and uses [`set_goal`]).
///
/// # Errors
///
/// What to tell the person when the command is refused.
pub fn apply(
    slot: &mut Option<Goal>,
    command: GoalCommand,
    ctx: Context<'_>,
) -> Result<Outcome, String> {
    let limits = ctx.limits;
    if ctx.running && !command.runs_while_busy() {
        return Err(crate::command::BUSY_SET.to_owned());
    }
    match command {
        GoalCommand::Set(text) | GoalCommand::Draft(text) => Ok(set(slot, &text)),
        GoalCommand::Status => Ok(Outcome::says(slot.as_ref().map_or_else(
            || "No goal set.".to_owned(),
            |g| g.status_text(limits.goal_max_turns),
        ))),
        GoalCommand::Show => Ok(Outcome::says(show(slot.as_ref()))),
        GoalCommand::Clear => Ok(clear(slot)),
        GoalCommand::Wait { pid, reason } => park(slot, pid, reason, ctx),
        other => control(goal_mut(slot)?, other),
    }
}

/// Sets `text` (with any inline contract) as the new goal: subgoals, gates
/// and barriers of a replaced goal go.
fn set(slot: &mut Option<Goal>, text: &str) -> Outcome {
    let (headline, contract) = crate::contract::parse_inline(text);
    set_goal(slot, Goal::new(headline, contract))
}

/// Sets `goal` (a drafted one keeps its contract); its first turn starts
/// at the next idle boundary.
#[must_use]
pub fn set_goal(slot: &mut Option<Goal>, goal: Goal) -> Outcome {
    let mut text = format!("Goal set: {}", goal.text);
    if let Some(contract) = goal.contract.as_ref().filter(|c| !c.is_empty()) {
        let _ = write!(text, "\n{}", contract.render());
    }
    *slot = Some(goal);
    Outcome::changed(text)
}

fn show(goal: Option<&Goal>) -> String {
    let Some(goal) = goal else {
        return "No goal set.".to_owned();
    };
    let mut text = format!("Goal: {}\nstatus: {}", goal.text, goal.status.as_str());
    match goal.contract.as_ref().filter(|c| !c.is_empty()) {
        Some(contract) => {
            let _ = write!(text, "\n{}", contract.render());
        }
        None => text.push_str("\n(no completion contract)"),
    }
    for (n, subgoal) in goal.subgoals.iter().enumerate() {
        let _ = write!(text, "\nsubgoal {}: {subgoal}", n + 1);
    }
    for (n, gate) in goal.gates.iter().enumerate() {
        let _ = write!(text, "\ngate {}: {}", n + 1, gate.command);
    }
    text
}

fn clear(slot: &mut Option<Goal>) -> Outcome {
    match slot.take() {
        Some(_) => Outcome::changed("Goal cleared."),
        None => Outcome::says("No goal set."),
    }
}

fn goal_mut(slot: &mut Option<Goal>) -> Result<&mut Goal, String> {
    slot.as_mut().ok_or_else(|| NO_GOAL.to_owned())
}

/// `/goal wait <pid> [reason]`: only a live local process.
fn park(
    slot: &mut Option<Goal>,
    pid: u32,
    reason: String,
    ctx: Context<'_>,
) -> Result<Outcome, String> {
    let goal = goal_mut(slot)?;
    let start_time = ctx
        .probe
        .pid_start(pid)
        .ok_or_else(|| format!("process {pid} is not running"))?;
    let barrier = Barrier::Pid {
        pid,
        start_time,
        until_ms: ctx.now_ms + secs_ms(ctx.limits.max_wait_secs),
        reason,
    };
    let text = format!("Goal parked on {}", barrier.describe());
    goal.barrier = Some(barrier);
    Ok(Outcome::changed(text))
}

fn control(goal: &mut Goal, command: GoalCommand) -> Result<Outcome, String> {
    let text = match command {
        GoalCommand::Pause => {
            goal.status = GoalStatus::Paused;
            goal.barrier = None;
            "Goal paused. Use /goal resume to keep going."
        }
        GoalCommand::Resume => {
            goal.status = GoalStatus::Active;
            goal.turns_used = 0;
            goal.barrier = None;
            goal.kickoff = true;
            for gate in &mut goal.gates {
                gate.failures = 0;
            }
            "Goal resumed."
        }
        GoalCommand::Unwait => {
            if goal.barrier.take().is_none() {
                return Ok(Outcome::says("The goal is not parked."));
            }
            "Goal unparked."
        }
        other => return gate_command(goal, other),
    };
    Ok(Outcome::changed(text))
}

fn gate_command(goal: &mut Goal, command: GoalCommand) -> Result<Outcome, String> {
    match command {
        GoalCommand::GateAdd(command) => {
            goal.gates.push(Gate {
                command: command.clone(),
                failures: 0,
            });
            Ok(Outcome::changed(format!(
                "Quality gate {} added: {command}",
                goal.gates.len()
            )))
        }
        GoalCommand::GateList => Ok(Outcome::says(numbered(
            "No quality gates.",
            goal.gates.iter().map(|g| g.command.as_str()),
        ))),
        GoalCommand::GateRemove(n) => {
            if n > goal.gates.len() {
                return Err(format!("there is no quality gate {n}"));
            }
            let gate = goal.gates.remove(n - 1);
            Ok(Outcome::changed(format!(
                "Quality gate {n} removed: {}",
                gate.command
            )))
        }
        GoalCommand::GateClear => {
            goal.gates.clear();
            Ok(Outcome::changed("Quality gates cleared."))
        }
        _ => Err(crate::command::GOAL_USAGE.to_owned()),
    }
}

fn numbered<'a>(empty: &str, items: impl Iterator<Item = &'a str>) -> String {
    let lines: Vec<String> = items
        .enumerate()
        .map(|(n, item)| format!("{}. {item}", n + 1))
        .collect();
    if lines.is_empty() {
        empty.to_owned()
    } else {
        lines.join("\n")
    }
}

/// Applies a `/subgoal` command (never resetting the turn counter).
///
/// # Errors
///
/// `No active goal.`, or a bad index.
pub fn subgoal(slot: &mut Option<Goal>, command: SubgoalCommand) -> Result<Outcome, String> {
    let goal = slot
        .as_mut()
        .filter(|g| g.status != GoalStatus::Done)
        .ok_or_else(|| NO_GOAL.to_owned())?;
    match command {
        SubgoalCommand::Add(text) => {
            goal.subgoals.push(text.clone());
            Ok(Outcome::changed(format!(
                "Subgoal {} added: {text}",
                goal.subgoals.len()
            )))
        }
        SubgoalCommand::List => Ok(Outcome::says(numbered(
            "No subgoals.",
            goal.subgoals.iter().map(String::as_str),
        ))),
        SubgoalCommand::Remove(n) => {
            if n > goal.subgoals.len() {
                return Err(format!("there is no subgoal {n}"));
            }
            let removed = goal.subgoals.remove(n - 1);
            Ok(Outcome::changed(format!("Subgoal {n} removed: {removed}")))
        }
        SubgoalCommand::Clear => {
            goal.subgoals.clear();
            Ok(Outcome::changed("Subgoals cleared."))
        }
    }
}

#[cfg(test)]
mod tests;
