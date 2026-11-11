//! Idle-boundary arbitration (spec: session-automation, Idle-boundary
//! arbitration). Automation fires only when the session is idle: no turn
//! runs and no user input waits (the host checks that before asking).
//! Precedence is a queued user message, then an active goal's
//! continuation, then a loop wakeup, then a heartbeat; while a goal is
//! active and not parked, loops and heartbeats wait, and at most one
//! automated turn is admitted per idle boundary.
//!
//! A goal's continuation after a turn comes from the judge (the host runs
//! it at the turn's end); [`due`] covers what a timer brings: a parked
//! goal's barrier releasing, a loop wakeup and a heartbeat fire.

use pmt_core::text::Catalog;

use crate::cadence;
use crate::goal::{self, Probe, Step};
use crate::state::{Automation, Kind, Limits, RunStatus};

/// An automated turn to admit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wakeup {
    /// Whose turn it is (`origin` of the stored user message).
    pub kind: Kind,
    /// The user-role text.
    pub prompt: String,
    /// A status line to show first.
    pub notice: Option<String>,
}

impl Wakeup {
    /// A goal continuation from a step (`None` unless it continues; the
    /// notice of any other step is the caller's to show).
    #[must_use]
    pub fn goal(step: &Step) -> Option<Self> {
        match step {
            Step::Continue { prompt, notice } => Some(Self {
                kind: Kind::Goal,
                prompt: prompt.clone(),
                notice: Some(notice.clone()),
            }),
            _ => None,
        }
    }
}

/// What a timer brings at `now_ms` on an idle session: the wakeup, with
/// `automation` already updated as if it were admitted (keep a copy to
/// refund a refused admission), and the status line of a goal step that
/// does not continue (worded with `text`).
pub fn due(
    text: &dyn Catalog,
    automation: &mut Automation,
    now_ms: i64,
    limits: &Limits,
    probe: &dyn Probe,
) -> (Option<Wakeup>, Option<String>) {
    if let Some(goal) = automation.goal.as_mut() {
        if goal.status == crate::state::GoalStatus::Active && goal.kickoff {
            goal.kickoff = false;
            let wakeup = Wakeup {
                kind: Kind::Goal,
                prompt: goal.kickoff_prompt(),
                notice: None,
            };
            return (Some(wakeup), None);
        }
        if let Some(step) = goal::release(text, goal, limits, now_ms, probe) {
            let wakeup = Wakeup::goal(&step);
            let notice = wakeup.is_none().then(|| step.notice().to_owned());
            return (wakeup, notice);
        }
        if goal.drives() {
            return (None, None);
        }
    }
    if let Some(looping) = automation
        .looping
        .as_mut()
        .filter(|l| l.status == RunStatus::Active && l.next_fire_ms <= now_ms)
    {
        match cadence::exhausted(text, looping, limits) {
            Some(cadence::LoopEnd::Removed(notice)) => {
                automation.looping = None;
                return (None, Some(notice));
            }
            Some(end) => return (None, Some(end_notice(end))),
            None => {}
        }
        cadence::fire_loop(looping, now_ms);
        let prompt = cadence::loop_prompt(looping);
        return (
            Some(Wakeup {
                kind: Kind::Loop,
                prompt,
                notice: None,
            }),
            None,
        );
    }
    if let Some(heartbeat) = automation
        .heartbeat
        .as_mut()
        .filter(|h| h.status == RunStatus::Active && h.next_fire_ms <= now_ms)
    {
        cadence::fire_heartbeat(heartbeat, now_ms);
        return (
            Some(Wakeup {
                kind: Kind::Heartbeat,
                prompt: cadence::heartbeat_prompt(&heartbeat.prompt),
                notice: None,
            }),
            None,
        );
    }
    (None, None)
}

fn end_notice(end: cadence::LoopEnd) -> String {
    match end {
        cadence::LoopEnd::Removed(notice) | cadence::LoopEnd::Paused(notice) => notice,
        cadence::LoopEnd::Next => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::LoopSpec;
    use crate::judge::{Verdict, WaitOn};
    use crate::state::Goal;

    /// The bundled English strings (what the assertions read).
    fn en() -> std::sync::Arc<pmt_i18n::Strings> {
        pmt_i18n::Strings::english(pmt_i18n::Surface::Core)
    }

    struct Pids(Vec<u32>);

    impl Probe for Pids {
        fn pid_start(&self, pid: u32) -> Option<u64> {
            self.0.contains(&pid).then_some(1)
        }

        fn process(&self, _: &str) -> Option<u32> {
            None
        }
    }

    fn looping(every: u64) -> crate::state::Loop {
        cadence::new_loop(
            LoopSpec {
                every_secs: Some(every),
                prompt: "check the queue".into(),
                times: None,
                until: None,
            },
            0,
            &Limits::default(),
        )
    }

    #[test]
    fn a_parked_goal_lets_the_loop_run() {
        let limits = Limits::default();
        let live = Pids(vec![4242]);
        let mut goal = Goal::new("ship".into(), None);
        goal.kickoff = false;
        goal::verdict(
            &*en(),
            &mut goal,
            Ok(Verdict::Wait {
                on: WaitOn::Pid(4242),
                reason: "CI".into(),
            }),
            &limits,
            0,
            &live,
        );
        let mut automation = Automation {
            goal: Some(goal),
            looping: Some(looping(120)),
            heartbeat: None,
        };
        // The first wakeup is due at once, the next two minutes later.
        let (first, _) = due(&*en(), &mut automation, 0, &limits, &live);
        assert_eq!(first.unwrap().kind, Kind::Loop);
        assert_eq!(due(&*en(), &mut automation, 60_000, &limits, &live).0, None);
        let (second, _) = due(&*en(), &mut automation, 120_000, &limits, &live);
        assert_eq!(second.unwrap().kind, Kind::Loop);
        assert!(automation.goal.as_ref().unwrap().barrier.is_some());
    }

    #[test]
    fn an_active_goal_defers_loops_and_heartbeats() {
        let limits = Limits::default();
        let mut automation = Automation {
            goal: Some(Goal {
                kickoff: false,
                ..Goal::new("ship".into(), None)
            }),
            looping: Some(looping(120)),
            heartbeat: Some(cadence::new_heartbeat(60, "ping".into(), 0)),
        };
        assert_eq!(
            due(&*en(), &mut automation, 999_999, &limits, &Pids(vec![])).0,
            None
        );
        automation.goal.as_mut().unwrap().status = crate::state::GoalStatus::Paused;
        let (wakeup, _) = due(&*en(), &mut automation, 999_999, &limits, &Pids(vec![]));
        assert_eq!(wakeup.unwrap().kind, Kind::Loop);
        // One automated turn per boundary: the heartbeat waits for the next.
        let (wakeup, _) = due(&*en(), &mut automation, 999_999, &limits, &Pids(vec![]));
        assert_eq!(wakeup.unwrap().kind, Kind::Heartbeat);
    }

    #[test]
    fn a_new_goal_starts_at_the_next_idle_boundary_before_a_loop() {
        let limits = Limits::default();
        let mut automation = Automation {
            goal: Some(Goal::new("ship".into(), None)),
            looping: Some(looping(120)),
            heartbeat: None,
        };
        let (wakeup, _) = due(&*en(), &mut automation, 0, &limits, &Pids(vec![]));
        let wakeup = wakeup.unwrap();
        assert_eq!((wakeup.kind, wakeup.prompt.as_str()), (Kind::Goal, "ship"));
        assert_eq!(automation.goal.as_ref().unwrap().turns_used, 0);
        // Then the goal drives: the loop waits for it.
        assert_eq!(
            due(&*en(), &mut automation, 0, &limits, &Pids(vec![])).0,
            None
        );
    }

    #[test]
    fn a_spent_loop_ends_before_another_wakeup_fires() {
        let limits = Limits::default();
        let mut spent = looping(30);
        spent.times = Some(1);
        spent.fired = 1;
        let mut automation = Automation {
            looping: Some(spent),
            ..Automation::default()
        };
        let (wakeup, notice) = due(&*en(), &mut automation, 0, &limits, &Pids(vec![]));
        assert_eq!(wakeup, None);
        assert_eq!(notice.as_deref(), Some("Loop finished after 1 runs."));
        assert!(automation.looping.is_none());
    }

    /// Scenario: Missed ticks coalesce, across a daemon restart: the
    /// stored heartbeat is read back by a new process whose (fake) clock is
    /// 35 minutes on.
    #[test]
    fn a_stored_heartbeat_fires_once_after_a_restart_and_re_anchors() {
        let limits = Limits::default();
        let minute = 60_000;
        let set_at = 1_000 * minute;
        let stored =
            serde_json::to_string(&cadence::new_heartbeat(600, "check CI".into(), set_at)).unwrap();
        let idle_at = set_at + 35 * minute;
        let mut automation = Automation {
            heartbeat: Some(serde_json::from_str(&stored).unwrap()),
            ..Automation::default()
        };
        let (wakeup, _) = due(&*en(), &mut automation, idle_at, &limits, &Pids(vec![]));
        assert_eq!(wakeup.unwrap().kind, Kind::Heartbeat);
        assert_eq!(
            due(&*en(), &mut automation, idle_at, &limits, &Pids(vec![])).0,
            None
        );
        assert_eq!(automation.next_fire_ms(), Some(idle_at + 10 * minute));
    }

    #[test]
    fn a_released_barrier_continues_the_goal() {
        let limits = Limits::default();
        let mut goal = Goal::new("ship".into(), None);
        goal.kickoff = false;
        goal::verdict(
            &*en(),
            &mut goal,
            Ok(Verdict::Wait {
                on: WaitOn::Seconds(30),
                reason: String::new(),
            }),
            &limits,
            0,
            &Pids(vec![]),
        );
        let mut automation = Automation {
            goal: Some(goal),
            ..Automation::default()
        };
        assert_eq!(automation.next_fire_ms(), Some(30_000));
        assert_eq!(
            due(&*en(), &mut automation, 29_999, &limits, &Pids(vec![])).0,
            None
        );
        let (wakeup, _) = due(&*en(), &mut automation, 30_000, &limits, &Pids(vec![]));
        let wakeup = wakeup.unwrap();
        assert_eq!(wakeup.kind, Kind::Goal);
        assert!(
            wakeup
                .notice
                .unwrap()
                .starts_with("Continuing toward goal (1/20)")
        );
    }
}
