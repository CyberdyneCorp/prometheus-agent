//! `/goal`, `/subgoal`, `/loop` (`/proactive`) and `/heartbeat` (`/hb`)
//! on every surface (spec: session-automation, Goal command surface; Goal
//! changes while a turn runs; Subgoals; Loop command and cadence; Session
//! heartbeat). The commands only change stored state; the host's next
//! idle boundary starts what they set (a new goal's first turn, a loop's
//! first wakeup), so no command races a running turn.

use pmt_automation::command::{self, GoalCommand, HeartbeatCommand, LoopCommand};
use pmt_automation::goal::{self, Context, Outcome};
use pmt_automation::state::{Goal, Route, RunStatus};
use pmt_automation::{Kind, cadence, judge};
use pmt_core::error::{Error, codes};

use super::{LocalProbe, OWNER_GATEWAY, Stored, limits, modify, now_ms};
use crate::chat::commands::Ctx;
use crate::slash::Surface;

fn usage(message: impl Into<String>) -> Error {
    Error::new(&codes::CLI_USAGE, message)
}

/// The surface a command sets automation for: the messaging gateway, or
/// the surface that created the session (`cli`, `tui`, `desktop`...).
fn owner(ctx: &Ctx<'_>) -> String {
    if ctx.surface == Surface::Gateway {
        OWNER_GATEWAY.to_owned()
    } else {
        ctx.live.surface().to_owned()
    }
}

/// The conversation a messaging session posts to.
fn route(ctx: &Ctx<'_>) -> Option<Route> {
    let session = ctx.factory.store.session(ctx.live.id).ok()?;
    match session.origin {
        pmt_store::Origin::Gateway {
            platform,
            chat,
            thread,
            user,
        } => Some(Route {
            platform,
            chat_id: chat,
            thread_id: thread,
            sender_id: user.unwrap_or_default(),
        }),
        _ => None,
    }
}

/// Runs `change` on the session's stored automation, recording `kind` as
/// set from this surface when it changed it.
fn change<T>(
    ctx: &Ctx<'_>,
    kind: Kind,
    change: impl FnOnce(&mut Stored) -> (T, bool),
) -> Result<T, Error> {
    let owner = owner(ctx);
    modify(&ctx.factory.store, ctx.live.id, |stored| {
        let (out, changed) = change(stored);
        if changed {
            stored.owners.insert(kind, owner);
        }
        out
    })
}

/// `/goal …`.
///
/// # Errors
///
/// The refusal to show (usage, a turn running, no goal, a dead pid).
pub async fn goal(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let parsed = command::goal(args).map_err(usage)?;
    let scope = ctx.live.config.get();
    let limits = limits(scope.config());
    if ctx.running && !parsed.runs_while_busy() {
        return Err(usage(command::BUSY_SET));
    }
    if let GoalCommand::Draft(objective) = &parsed {
        let drafted = draft(ctx, objective).await;
        return set_drafted(ctx, drafted);
    }
    let route = route(ctx);
    let probe = LocalProbe::of(ctx.live.id);
    let outcome = change(ctx, Kind::Goal, |stored| {
        let context = Context {
            limits: &limits,
            running: ctx.running,
            now_ms: now_ms(),
            probe: &probe,
        };
        let outcome = goal::apply(&mut stored.automation.goal, parsed, context);
        if let (Some(goal), Ok(o)) = (stored.automation.goal.as_mut(), &outcome)
            && o.changed
        {
            goal.route.clone_from(&route);
        }
        let changed = outcome.as_ref().is_ok_and(|o| o.changed);
        (outcome, changed)
    })?;
    outcome.map(|o: Outcome| o.text).map_err(usage)
}

/// `/goal draft <objective>`: the judge expands it into a contract; when
/// it cannot, the objective becomes a free-form goal.
async fn draft(ctx: &Ctx<'_>, objective: &str) -> Goal {
    let judge = ctx.factory.models.judge(&ctx.live.model, ctx.live.id);
    let contract = match judge.ask(&judge::draft_prompt(objective)).await {
        Ok(reply) => judge::parse_draft(&reply),
        Err(err) => Err(err),
    };
    match contract {
        Ok(contract) => Goal::new(objective.to_owned(), Some(contract)),
        Err(err) => {
            tracing::warn!(
                error = err.as_str(),
                "goal draft failed; free-form goal set"
            );
            Goal::new(objective.to_owned(), None)
        }
    }
}

fn set_drafted(ctx: &Ctx<'_>, drafted: Goal) -> Result<String, Error> {
    let route = route(ctx);
    change(ctx, Kind::Goal, |stored| {
        let goal = Goal { route, ..drafted };
        (goal::set_goal(&mut stored.automation.goal, goal).text, true)
    })
}

/// `/subgoal …`.
///
/// # Errors
///
/// `No active goal.`, or a bad index.
pub fn subgoal(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let parsed = command::subgoal(args).map_err(usage)?;
    let outcome = change(ctx, Kind::Goal, |stored| {
        let outcome = goal::subgoal(&mut stored.automation.goal, parsed);
        let changed = outcome.as_ref().is_ok_and(|o| o.changed);
        (outcome, changed)
    })?;
    outcome.map(|o| o.text).map_err(usage)
}

/// `/loop …` (alias `/proactive`).
///
/// # Errors
///
/// The usage line, an interval below `loops.min_interval_secs`.
pub fn looping(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let scope = ctx.live.config.get();
    let limits = limits(scope.config());
    let parsed = command::looping(args, limits.loop_min_interval_secs).map_err(usage)?;
    let route = route(ctx);
    let now = now_ms();
    change(ctx, Kind::Loop, |stored| {
        let slot = &mut stored.automation.looping;
        match parsed {
            LoopCommand::Set(spec) => {
                let mut looping = cadence::new_loop(spec, now, &limits);
                looping.route = route;
                let text = format!("Loop set: {}", looping.row(now));
                *slot = Some(looping);
                (Ok(text), true)
            }
            LoopCommand::Status => (
                Ok(slot
                    .as_ref()
                    .map_or_else(|| "No loop.".to_owned(), |l| l.row(now))),
                false,
            ),
            LoopCommand::Pause => match slot.as_mut() {
                Some(looping) => {
                    looping.status = RunStatus::Paused;
                    (Ok("Loop paused.".to_owned()), true)
                }
                None => (Err(usage("No loop.")), false),
            },
            LoopCommand::Resume => match slot.as_mut() {
                Some(looping) => {
                    cadence::resume_loop(looping, now);
                    (Ok("Loop resumed.".to_owned()), true)
                }
                None => (Err(usage("No loop.")), false),
            },
            LoopCommand::Stop => match slot.take() {
                Some(_) => (Ok("Loop stopped.".to_owned()), true),
                None => (Ok("No loop.".to_owned()), false),
            },
        }
    })?
}

/// `/heartbeat …` (alias `/hb`).
///
/// # Errors
///
/// The usage line, an interval below `heartbeat.min_interval_secs`.
pub fn heartbeat(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let scope = ctx.live.config.get();
    let limits = limits(scope.config());
    let parsed = command::heartbeat(args, limits.heartbeat_min_interval_secs).map_err(usage)?;
    let route = route(ctx);
    let now = now_ms();
    change(ctx, Kind::Heartbeat, |stored| {
        let slot = &mut stored.automation.heartbeat;
        match parsed {
            HeartbeatCommand::Set { every_secs, prompt } => {
                let mut heartbeat = cadence::new_heartbeat(every_secs, prompt, now);
                heartbeat.route = route;
                let text = format!("Heartbeat set: {}", heartbeat.row(now));
                *slot = Some(heartbeat);
                (Ok(text), true)
            }
            HeartbeatCommand::Status => (
                Ok(slot
                    .as_ref()
                    .map_or_else(|| "No heartbeat.".to_owned(), |h| h.row(now))),
                false,
            ),
            HeartbeatCommand::Pause => match slot.as_mut() {
                Some(heartbeat) => {
                    heartbeat.status = RunStatus::Paused;
                    (Ok("Heartbeat paused.".to_owned()), true)
                }
                None => (Err(usage("No heartbeat.")), false),
            },
            HeartbeatCommand::Resume => match slot.as_mut() {
                Some(heartbeat) => {
                    cadence::resume_heartbeat(heartbeat, now);
                    (
                        Ok(format!("Heartbeat resumed: {}", heartbeat.row(now))),
                        true,
                    )
                }
                None => (Err(usage("No heartbeat.")), false),
            },
            HeartbeatCommand::Clear => match slot.take() {
                Some(_) => (Ok("Heartbeat cleared.".to_owned()), true),
                None => (Ok("No heartbeat.".to_owned()), false),
            },
        }
    })?
}
