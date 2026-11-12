//! Goal transitions against a scripted judge and a fake process table.

use super::*;
use crate::command::goal as parse;

/// The bundled English strings (what the assertions read).
fn en() -> std::sync::Arc<pmt_i18n::Strings> {
    pmt_i18n::Strings::english(pmt_i18n::Surface::Core)
}

/// The same strings, borrowed for as long as the test runs.
fn english() -> &'static pmt_i18n::Strings {
    Box::leak(Box::new(en()))
}

/// Pids that run (with start time 7) and session processes (with their
/// watch matches).
#[derive(Default)]
struct Table {
    pids: Vec<u32>,
    processes: Vec<(String, u32)>,
}

impl Probe for Table {
    fn pid_start(&self, pid: u32) -> Option<u64> {
        self.pids.contains(&pid).then_some(7)
    }

    fn process(&self, id: &str) -> Option<u32> {
        self.processes
            .iter()
            .find(|(p, _)| p == id)
            .map(|(_, m)| *m)
    }
}

fn limits(max_turns: u32) -> Limits {
    Limits {
        goal_max_turns: max_turns,
        ..Limits::default()
    }
}

fn ctx<'a>(limits: &'a Limits, probe: &'a Table, running: bool) -> Context<'a> {
    Context {
        limits,
        running,
        now_ms: 1_000,
        probe,
        text: english(),
    }
}

fn active(text: &str) -> Goal {
    Goal::new(text.into(), None)
}

#[test]
fn continue_injects_the_next_turn_and_counts_it() {
    let mut goal = active("port the parser");
    let step = verdict(
        &*en(),
        &mut goal,
        Ok(Verdict::Continue("3 files remain".into())),
        &limits(20),
        0,
        &Table::default(),
    );
    let Step::Continue { prompt, notice } = step else {
        panic!("{step:?}")
    };
    assert_eq!(notice, "Continuing toward goal (1/20): 3 files remain");
    assert!(prompt.contains("Goal: port the parser"));
    assert_eq!(goal.turns_used, 1);
}

#[test]
fn a_broken_judge_fails_open_and_advances_the_counter() {
    let mut goal = active("x");
    let step = verdict(
        &*en(),
        &mut goal,
        Err("HTTP 500".into()),
        &limits(20),
        0,
        &Table::default(),
    );
    assert!(matches!(step, Step::Continue { .. }), "{step:?}");
    assert_eq!(goal.turns_used, 1);
}

#[test]
fn budget_exhaustion_pauses_without_a_fourth_continuation() {
    let mut goal = active("x");
    let limits = limits(3);
    let mut continued = 0;
    loop {
        match verdict(
            &*en(),
            &mut goal,
            Ok(Verdict::Continue(String::new())),
            &limits,
            0,
            &Table::default(),
        ) {
            Step::Continue { .. } => continued += 1,
            Step::Paused(notice) => {
                assert_eq!(notice, budget_notice(&*en(), 3));
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(continued, 3);
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(
        budget_notice(&*en(), 20),
        "Goal paused — 20/20 turns used. Use /goal resume to keep going, or /goal clear to stop."
    );
}

#[test]
fn a_blocked_verdict_pauses_and_is_never_achieved() {
    let mut goal = active("x");
    let step = verdict(
        &*en(),
        &mut goal,
        Ok(Verdict::Blocked(
            "requires production DB credentials".into(),
        )),
        &limits(20),
        0,
        &Table::default(),
    );
    assert_eq!(
        step,
        Step::Paused(
            "Goal judged unachievable — paused: requires production DB credentials".into()
        )
    );
    assert_eq!(goal.status, GoalStatus::Paused);
    assert_eq!(
        goal.reason.as_deref(),
        Some("requires production DB credentials")
    );
}

#[test]
fn resume_resets_the_counter_and_a_new_goal_drops_subgoals_gates_and_barriers() {
    let table = Table {
        pids: vec![4242],
        ..Table::default()
    };
    let limits = limits(20);
    let mut slot = Some(active("old"));
    let goal = slot.as_mut().unwrap();
    goal.turns_used = 20;
    goal.status = GoalStatus::Paused;
    goal.subgoals.push("s".into());
    goal.gates.push(Gate {
        command: "true".into(),
        failures: 2,
    });
    apply(&mut slot, GoalCommand::Resume, ctx(&limits, &table, false)).unwrap();
    let goal = slot.as_ref().unwrap();
    assert_eq!((goal.turns_used, goal.status), (0, GoalStatus::Active));
    apply(
        &mut slot,
        parse("wait 4242").unwrap(),
        ctx(&limits, &table, false),
    )
    .unwrap();
    let outcome = apply(
        &mut slot,
        parse("new goal").unwrap(),
        ctx(&limits, &table, false),
    )
    .unwrap();
    assert_eq!(outcome.text, "Goal set: new goal");
    let goal = slot.unwrap();
    assert!(goal.kickoff);
    assert!(goal.subgoals.is_empty() && goal.gates.is_empty() && goal.barrier.is_none());
}

#[test]
fn a_new_goal_is_refused_while_a_turn_runs_but_control_commands_run() {
    let table = Table::default();
    let limits = limits(20);
    let mut slot = Some(active("keep"));
    assert_eq!(
        apply(
            &mut slot,
            parse("write the report").unwrap(),
            ctx(&limits, &table, true)
        ),
        Err("A turn is running; use /stop first, then set the new goal.".into())
    );
    assert_eq!(slot.as_ref().unwrap().text, "keep");
    let outcome = apply(
        &mut slot,
        parse("pause").unwrap(),
        ctx(&limits, &table, true),
    )
    .unwrap();
    assert!(outcome.changed);
    assert!(
        apply(
            &mut slot,
            parse("status").unwrap(),
            ctx(&limits, &table, true)
        )
        .is_ok()
    );
}

#[test]
fn a_red_gate_skips_the_judge_and_carries_the_exit_and_output_tail() {
    let mut goal = active("green tests");
    goal.gates.push(Gate {
        command: "cargo test".into(),
        failures: 0,
    });
    let output = format!("{}tail of the failure", "y".repeat(5_000));
    let step = gates(
        &*en(),
        &mut goal,
        &[GateRun { exit: 101, output }],
        &limits(20),
    );
    let Some(Step::Continue { prompt, .. }) = step else {
        panic!("{step:?}")
    };
    assert!(prompt.contains("exit 101"));
    assert!(prompt.contains("tail of the failure"));
    assert!(!prompt.contains(&"y".repeat(GATE_OUTPUT_TAIL)));
}

#[test]
fn an_exhausted_gate_pauses_naming_the_gate() {
    let mut goal = active("green tests");
    goal.gates.push(Gate {
        command: "cargo test".into(),
        failures: 0,
    });
    let red = [GateRun {
        exit: 101,
        output: String::new(),
    }];
    let limits = limits(20);
    assert!(matches!(
        gates(&*en(), &mut goal, &red, &limits),
        Some(Step::Continue { .. })
    ));
    assert!(matches!(
        gates(&*en(), &mut goal, &red, &limits),
        Some(Step::Continue { .. })
    ));
    let Some(Step::Paused(notice)) = gates(&*en(), &mut goal, &red, &limits) else {
        panic!("not paused")
    };
    assert!(notice.contains("`cargo test`"), "{notice}");
    assert!(notice.contains("/goal gate remove 1"), "{notice}");
    assert!(notice.contains("/goal resume"), "{notice}");
    assert_eq!(goal.status, GoalStatus::Paused);
    let green = [GateRun {
        exit: 0,
        output: String::new(),
    }];
    assert_eq!(gates(&*en(), &mut goal, &green, &limits), None);
    assert_eq!(goal.gates[0].failures, 0);
}

#[test]
fn a_wait_on_a_live_pid_parks_and_a_dead_one_continues() {
    let table = Table {
        pids: vec![4242],
        ..Table::default()
    };
    let limits = limits(20);
    let mut goal = active("ship");
    let step = verdict(
        &*en(),
        &mut goal,
        Ok(Verdict::Wait {
            on: WaitOn::Pid(4242),
            reason: "CI running".into(),
        }),
        &limits,
        0,
        &table,
    );
    assert_eq!(
        step,
        Step::Parked("Goal parked on pid 4242 (CI running)".into())
    );
    assert!(!goal.drives());
    assert_eq!(goal.turns_used, 0);
    assert!(goal.status_text(&*en(), 20).contains("parked on pid 4242"));
    assert_eq!(release(&*en(), &mut goal, &limits, 1_000, &table), None);
    assert!(matches!(
        release(&*en(), &mut goal, &limits, 1_000, &Table::default()),
        Some(Step::Continue { .. })
    ));
    let mut other = active("ship");
    let step = verdict(
        &*en(),
        &mut other,
        Ok(Verdict::Wait {
            on: WaitOn::Pid(99_999),
            reason: String::new(),
        }),
        &limits,
        0,
        &table,
    );
    assert!(matches!(step, Step::Continue { .. }), "{step:?}");
}

#[test]
fn a_pid_or_session_wait_is_capped() {
    let table = Table {
        processes: vec![("proc_1".into(), 0)],
        ..Table::default()
    };
    let limits = limits(20);
    let mut goal = active("ship");
    verdict(
        &*en(),
        &mut goal,
        Ok(Verdict::Wait {
            on: WaitOn::Session("proc_1".into()),
            reason: String::new(),
        }),
        &limits,
        0,
        &table,
    );
    assert_eq!(goal.barrier.as_ref().unwrap().until_ms(), 1_800_000);
    assert_eq!(release(&*en(), &mut goal, &limits, 1_799_999, &table), None);
    let matched = Table {
        processes: vec![("proc_1".into(), 1)],
        ..Table::default()
    };
    assert!(release(&*en(), &mut goal, &limits, 10, &matched).is_some());
}

#[test]
fn a_dead_pid_is_refused_for_goal_wait() {
    let table = Table::default();
    let limits = limits(20);
    let mut slot = Some(active("ship"));
    assert_eq!(
        apply(
            &mut slot,
            parse("wait 99999").unwrap(),
            ctx(&limits, &table, false)
        ),
        Err("process 99999 is not running".into())
    );
    assert_eq!(slot.unwrap().status, GoalStatus::Active);
}

#[test]
fn subgoals_need_a_goal_and_keep_the_counter() {
    let mut slot = None;
    assert_eq!(
        subgoal(&*en(), &mut slot, SubgoalCommand::Add("x".into())),
        Err(NO_GOAL.into())
    );
    let mut goal = active("fix the failing tests");
    goal.turns_used = 2;
    slot = Some(goal);
    subgoal(
        &*en(),
        &mut slot,
        SubgoalCommand::Add("add a regression test".into()),
    )
    .unwrap();
    let goal = slot.as_ref().unwrap();
    assert_eq!(goal.turns_used, 2);
    let next = crate::judge::continuation(goal, "");
    assert!(
        next.contains("Additional criteria the user added mid-loop:\n1. add a regression test")
    );
}
