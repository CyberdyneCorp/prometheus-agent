//! Turn boundaries and idle wakeups of one session (spec:
//! session-automation, Goal judge loop; Quality gates; Goal wait
//! barriers; Loop stop conditions; Idle-boundary arbitration).
//!
//! [`after_turn`] runs when any turn of the session ends: a loop wakeup's
//! end updates the loop, then an active goal runs its gates and its judge
//! and may continue with one more (automated) turn. [`due`] runs when the
//! session is idle: a new goal's first turn, a parked goal's release, a
//! loop wakeup or a heartbeat, at most one, already reserved; a host that
//! cannot start it hands it back with [`refund`]. The judge and the gates
//! run outside the session's lock, and their result is applied only to the
//! goal they judged (a goal changed meanwhile is left alone).

use pmt_automation::arbiter::{self, Wakeup};
use pmt_automation::cadence::{self, LoopEnd};
use pmt_automation::goal::{self, Step};
use pmt_automation::judge;
use pmt_automation::state::Goal;
use pmt_automation::{Automation, Kind, Limits};
use pmt_core::id::SessionId;
use pmt_store::Store;

use super::{Host, LocalProbe, Stored, limits, modify, now_ms};
use crate::chat::session::{Factory, Live};

/// How long a goal parked on a process waits between liveness checks.
pub const BARRIER_POLL_MS: i64 = 2_000;

/// The session a boundary runs for.
pub struct Session<'a> {
    /// Its machinery (store, judge route).
    pub factory: &'a Factory,
    /// The session.
    pub live: &'a Live,
    /// Who drives it.
    pub host: Host,
}

/// How a turn ended.
#[derive(Debug, Clone, Copy)]
pub struct Ended<'a> {
    /// What started it (`None`: a person).
    pub origin: Option<Kind>,
    /// Its final reply.
    pub reply: &'a str,
    /// The person stopped it.
    pub interrupted: bool,
}

/// What a boundary decided.
#[derive(Debug, Default)]
pub struct Next {
    /// Status lines to show (on every surface).
    pub notices: Vec<String>,
    /// The goal's continuation turn, when it continues.
    pub wakeup: Option<Wakeup>,
}

/// A wakeup reserved by [`due`] (or a goal continuation, which reserves
/// nothing).
#[derive(Debug)]
pub struct Reserved {
    /// The turn to start.
    pub wakeup: Wakeup,
    /// The state before the reservation (a refund restores its kind).
    before: Option<Stored>,
    /// The state the reservation left (a kind changed since, by a command,
    /// is not refunded).
    left: Option<Stored>,
}

impl Reserved {
    /// A continuation the judge asked for: nothing to refund.
    #[must_use]
    pub fn continuation(wakeup: Wakeup) -> Self {
        Self {
            wakeup,
            before: None,
            left: None,
        }
    }
}

/// After a turn of the session ended.
pub async fn after_turn(session: &Session<'_>, ended: Ended<'_>) -> Next {
    let mut next = Next::default();
    let store = &session.factory.store;
    let id = session.live.id;
    let scope = session.live.config.get();
    let limits = limits(scope.config());
    let Ok(stored) = super::load(store, id) else {
        return next;
    };
    if ended.origin == Some(Kind::Loop)
        && stored.automation.looping.is_some()
        && session.host.drives(stored.owner(Kind::Loop))
    {
        loop_end(session, ended, &limits, &mut next).await;
    }
    let drives_goal = session.host.drives(stored.owner(Kind::Goal));
    let goal = stored
        .automation
        .goal
        .filter(|g| g.drives() && !g.kickoff && drives_goal);
    if let Some(goal) = goal {
        let timeout = scope.config().goals.gate_timeout_secs;
        let step = goal_boundary(session, &goal, ended.reply, (&limits, timeout)).await;
        match step {
            Some(step @ Step::Continue { .. }) => next.wakeup = Wakeup::goal(&step),
            Some(step) => next.notices.push(step.notice().to_owned()),
            None => {}
        }
    }
    next
}

async fn loop_end(session: &Session<'_>, ended: Ended<'_>, limits: &Limits, next: &mut Next) {
    let store = &session.factory.store;
    let id = session.live.id;
    let now = now_ms();
    let end = modify(store, id, |st| {
        let looping = st.automation.looping.as_mut()?;
        let end = cadence::after_loop_turn(looping, ended.reply, ended.interrupted, now, limits);
        let until = looping.until.clone();
        settle_loop(&mut st.automation, &end);
        Some((end, until))
    });
    let (end, until) = match end {
        Ok(Some(found)) => found,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(error = %err, "loop state not saved");
            return;
        }
    };
    let end = match (end, until) {
        (LoopEnd::Next, Some(condition)) => {
            let prompt = judge::until_prompt(&condition, ended.reply);
            let verdict = ask(session, &prompt).await.and_then(|t| judge::parse(&t));
            modify(store, id, |st| {
                let end = st
                    .automation
                    .looping
                    .as_mut()
                    .map_or(LoopEnd::Next, |l| cadence::until(l, verdict));
                settle_loop(&mut st.automation, &end);
                end
            })
            .unwrap_or(LoopEnd::Next)
        }
        (end, _) => end,
    };
    if let LoopEnd::Removed(notice) | LoopEnd::Paused(notice) = end {
        next.notices.push(notice);
    }
}

fn settle_loop(automation: &mut Automation, end: &LoopEnd) {
    if matches!(end, LoopEnd::Removed(_)) {
        automation.looping = None;
    }
}

/// Asks the session's judge, logging a failure at WARN (the caller fails
/// open).
async fn ask(session: &Session<'_>, prompt: &str) -> Result<String, String> {
    let judge = session
        .factory
        .models
        .judge(&session.live.model, session.live.id);
    judge.ask(prompt).await.inspect_err(|err| {
        tracing::warn!(error = err.as_str(), "goal judge failed; continuing");
    })
}

/// The goal `stored` holds when it is still the one `judged` was.
fn same<'a>(stored: &'a mut Stored, judged: &Goal) -> Option<&'a mut Goal> {
    stored.automation.goal.as_mut().filter(|g| {
        g.text == judged.text
            && g.status == judged.status
            && g.turns_used == judged.turns_used
            && g.barrier == judged.barrier
            && g.gates.len() == judged.gates.len()
            && g.gates
                .iter()
                .zip(&judged.gates)
                .all(|(a, b)| a.command == b.command)
    })
}

/// Gates, then (when they are green) the judge.
async fn goal_boundary(
    session: &Session<'_>,
    goal: &Goal,
    reply: &str,
    (limits, gate_timeout): (&Limits, u64),
) -> Option<Step> {
    let store = &session.factory.store;
    let id = session.live.id;
    let probe = LocalProbe::of(id);
    if !goal.gates.is_empty() {
        let runs = super::gates::run(id, &goal.gates, gate_timeout).await;
        let red = modify(store, id, |st| {
            same(st, goal).map(|g| goal::gates(g, &runs, limits))
        });
        match red {
            Ok(Some(Some(step))) => return Some(step),
            Ok(Some(None)) => {}
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!(error = %err, "goal state not saved");
                return None;
            }
        }
    }
    let prompt = judge::prompt(goal, reply, &probe.processes());
    let verdict = ask(session, &prompt).await.and_then(|t| {
        judge::parse(&t).inspect_err(|err| {
            tracing::warn!(error = err.as_str(), "unreadable goal verdict; continuing");
        })
    });
    let now = now_ms();
    modify(store, id, |st| {
        same(st, goal).map(|g| goal::verdict(g, verdict, limits, now, &probe))
    })
    .inspect_err(|err| tracing::warn!(error = %err, "goal state not saved"))
    .ok()
    .flatten()
}

/// The kinds `host` does not fire, taken out of `automation`.
fn hidden(stored: &mut Stored, host: Host) -> Automation {
    let mut out = Automation::default();
    if !host.drives(stored.owner(Kind::Goal)) {
        out.goal = stored.automation.goal.take();
    }
    if !host.drives(stored.owner(Kind::Loop)) {
        out.looping = stored.automation.looping.take();
    }
    if !host.drives(stored.owner(Kind::Heartbeat)) {
        out.heartbeat = stored.automation.heartbeat.take();
    }
    out
}

fn restore(stored: &mut Stored, hidden: Automation) {
    let automation = &mut stored.automation;
    automation.goal = hidden.goal.or(automation.goal.take());
    automation.looping = hidden.looping.or(automation.looping.take());
    automation.heartbeat = hidden.heartbeat.or(automation.heartbeat.take());
}

/// What is due on the idle session `id` now: the reserved wakeup (at most
/// one) and status lines (a goal that paused when its barrier released).
#[must_use]
pub fn due(
    store: &Store,
    id: SessionId,
    host: Host,
    limits: &Limits,
) -> (Option<Reserved>, Vec<String>) {
    let now = now_ms();
    let probe = LocalProbe::of(id);
    let found = modify(store, id, |st| {
        let before = st.clone();
        let hidden = hidden(st, host);
        let (wakeup, notice) = arbiter::due(&mut st.automation, now, limits, &probe);
        restore(st, hidden);
        let reserved = wakeup.map(|wakeup| Reserved {
            wakeup,
            before: Some(before),
            left: Some(st.clone()),
        });
        (reserved, notice)
    });
    match found {
        Ok((reserved, notice)) => (reserved, notice.into_iter().collect()),
        Err(err) => {
            tracing::warn!(error = %err, "automation state not read");
            (None, Vec::new())
        }
    }
}

/// Hands back a reservation whose turn did not start: its kind returns to
/// what it was before (no tick spent), unless a command changed it since.
pub fn refund(store: &Store, id: SessionId, reserved: Reserved) {
    let kind = reserved.wakeup.kind;
    let (Some(before), Some(left)) = (reserved.before, reserved.left) else {
        return;
    };
    let result = modify(store, id, |st| {
        let now = &mut st.automation;
        let (before, left) = (before.automation, left.automation);
        match kind {
            Kind::Goal if now.goal == left.goal => now.goal = before.goal,
            Kind::Loop if now.looping == left.looping => now.looping = before.looping,
            Kind::Heartbeat if now.heartbeat == left.heartbeat => {
                now.heartbeat = before.heartbeat;
            }
            _ => tracing::debug!(kind = kind.as_str(), "changed since reserved; not refunded"),
        }
    });
    if let Err(err) = result {
        tracing::warn!(error = %err, "automation refund not saved");
    }
}

/// When `host` should look at `id` next (ms since the epoch): a new
/// goal's first turn at once; while a goal drives nothing else (its turns
/// come from the judge); a parked goal's deadline, checked every
/// [`BARRIER_POLL_MS`] when it waits on a process; the earliest loop or
/// heartbeat fire.
#[must_use]
pub fn next_check(store: &Store, id: SessionId, host: Host) -> Option<i64> {
    let mut stored = super::load(store, id).ok()?;
    hidden(&mut stored, host);
    let automation = &stored.automation;
    let now = now_ms();
    let mut parked = None;
    if let Some(goal) = automation
        .goal
        .as_ref()
        .filter(|g| g.status == pmt_automation::GoalStatus::Active)
    {
        if goal.kickoff {
            return Some(now);
        }
        let barrier = goal.barrier.as_ref()?;
        parked = Some(match barrier {
            pmt_automation::state::Barrier::Seconds { until_ms, .. } => *until_ms,
            other => other.until_ms().min(now + BARRIER_POLL_MS),
        });
    }
    let fires = Automation {
        goal: None,
        ..automation.clone()
    };
    [fires.next_fire_ms(), parked].into_iter().flatten().min()
}

#[cfg(test)]
mod tests {
    use pmt_automation::command::LoopSpec;

    use super::*;

    fn store() -> (tempfile::TempDir, Store, SessionId) {
        let root = tempfile::tempdir().unwrap();
        let home = pmt_config::Home::open(root.path()).unwrap();
        let profile = home
            .profile(&pmt_config::ProfileName::default_profile())
            .unwrap();
        let scope = pmt_config::ProfileScope::open(
            profile,
            &pmt_config::Overrides::default(),
            pmt_config::Launch::Other,
        )
        .unwrap();
        let store = Store::open(&scope).unwrap();
        let id = store
            .create_session(&pmt_store::NewSession::new(pmt_store::Source::Cli))
            .unwrap()
            .id;
        (root, store, id)
    }

    fn set_loop(store: &Store, id: SessionId) {
        let spec = LoopSpec {
            every_secs: Some(60),
            prompt: "poll CI".into(),
            times: None,
            until: None,
        };
        modify(store, id, |st| {
            st.automation.looping = Some(cadence::new_loop(spec, now_ms(), &Limits::default()));
            st.owners.insert(Kind::Loop, "cli".into());
        })
        .unwrap();
    }

    fn looping(store: &Store, id: SessionId) -> Option<pmt_automation::state::Loop> {
        super::super::load(store, id).unwrap().automation.looping
    }

    /// Heartbeat ownership, recovery and silence on the gateway: a tick
    /// reserved at admission is refunded when the attempt is rejected
    /// before the turn starts.
    #[test]
    fn a_refused_wakeup_gives_its_tick_back() {
        let (_root, store, id) = store();
        set_loop(&store, id);
        let before = looping(&store, id);
        let (reserved, _) = due(&store, id, Host::Line, &Limits::default());
        let reserved = reserved.expect("the new loop's first wakeup is due");
        assert_eq!(looping(&store, id).unwrap().fired, 1);
        refund(&store, id, reserved);
        assert_eq!(looping(&store, id), before);
    }

    /// A refund never brings back a loop a command changed meanwhile.
    #[test]
    fn a_refund_leaves_a_loop_stopped_since_alone() {
        let (_root, store, id) = store();
        set_loop(&store, id);
        let (reserved, _) = due(&store, id, Host::Line, &Limits::default());
        modify(&store, id, |st| st.automation.looping = None).unwrap();
        refund(&store, id, reserved.unwrap());
        assert_eq!(looping(&store, id), None);
    }
}
