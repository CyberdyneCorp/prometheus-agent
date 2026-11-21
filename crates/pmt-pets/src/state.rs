//! The one activity-to-state mapping (spec: pets, One activity-to-state
//! mapping). Every surface feeds the same [`Tracker`] the events it
//! already receives (the trajectory events of `session.event`, the end of
//! each turn, the questions waiting for the user) and draws
//! [`Tracker::state`]; the desktop's `pet/state.ts` mirrors [`derive`].

use std::time::{Duration, Instant};

use serde_json::Value;

/// An animation state (a row of the atlas).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PetState {
    /// Nothing is happening.
    Idle,
    /// A turn just finished cleanly.
    Wave,
    /// A tool runs, or a turn is in flight.
    Run,
    /// A tool or turn just failed.
    Failed,
    /// The model is reasoning or reading.
    Review,
    /// A celebration: every todo of the plan is done, an achievement
    /// unlocked.
    Jump,
    /// A question waits for the user.
    Waiting,
}

impl PetState {
    /// Every state.
    pub const ALL: [Self; 7] = [
        Self::Idle,
        Self::Wave,
        Self::Run,
        Self::Failed,
        Self::Review,
        Self::Jump,
        Self::Waiting,
    ];

    /// The state's name (`pets show --state`, the desktop).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Wave => "wave",
            Self::Run => "run",
            Self::Failed => "failed",
            Self::Review => "review",
            Self::Jump => "jump",
            Self::Waiting => "waiting",
        }
    }

    /// The state a name spells.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }

    /// The atlas row (the petdex order: idle, running-right,
    /// running-left, waving, jumping, failed, waiting, running, review).
    #[must_use]
    pub const fn row(self) -> u32 {
        match self {
            Self::Idle => 0,
            Self::Wave => 3,
            Self::Jump => 4,
            Self::Failed => 5,
            Self::Waiting => 6,
            Self::Run => 7,
            Self::Review => 8,
        }
    }
}

/// The signals a state is derived from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent signals of one moment
pub struct Activity {
    /// A tool or the turn just failed.
    pub failed: bool,
    /// Every todo of the plan just completed, or an achievement unlocked.
    pub celebrate: bool,
    /// An approval or clarify request waits for the user.
    pub awaiting_user: bool,
    /// A tool is executing.
    pub tool_running: bool,
    /// The model streams reasoning or reads.
    pub reasoning: bool,
    /// A turn is in flight.
    pub busy: bool,
    /// A turn just finished cleanly.
    pub just_finished: bool,
}

/// The state of `activity`, in the specified precedence.
#[must_use]
pub fn derive(activity: Activity) -> PetState {
    let ranked = [
        (activity.failed, PetState::Failed),
        (activity.celebrate, PetState::Jump),
        (activity.awaiting_user, PetState::Waiting),
        (activity.tool_running, PetState::Run),
        (activity.reasoning, PetState::Review),
        (activity.busy, PetState::Run),
        (activity.just_finished, PetState::Wave),
    ];
    ranked
        .into_iter()
        .find_map(|(on, state)| on.then_some(state))
        .unwrap_or(PetState::Idle)
}

/// How long a momentary state (failed, jump, wave, the heart) shows.
pub const MOMENT: Duration = Duration::from_secs(3);

/// Follows one session's events and answers its pet state.
#[derive(Debug, Clone, Default)]
pub struct Tracker {
    tools_running: u32,
    reasoning: bool,
    busy: bool,
    waiting: u32,
    failed_at: Option<Instant>,
    celebrated_at: Option<Instant>,
    finished_at: Option<Instant>,
    heart_at: Option<Instant>,
}

impl Tracker {
    /// A trajectory event of the session (`session.event`'s `type` and
    /// `payload`).
    pub fn event(&mut self, kind: &str, payload: &Value, now: Instant) {
        match kind {
            "turn.start" => {
                self.busy = true;
                self.finished_at = None;
            }
            "assistant.reasoning" => self.reasoning = true,
            "tool.call" => {
                self.reasoning = false;
                self.tools_running += 1;
            }
            "tool.result" => self.tool_result(payload, now),
            "approval.request" | "clarify.request" => self.waiting += 1,
            "approval.decision" | "clarify.answer" => self.waiting = self.waiting.saturating_sub(1),
            "error" => self.failed_at = Some(now),
            "turn.end" => self.turn_ended(payload["reason"].as_str() == Some("error"), now),
            _ => {}
        }
    }

    fn tool_result(&mut self, payload: &Value, now: Instant) {
        self.tools_running = self.tools_running.saturating_sub(1);
        if payload["ok"] == Value::Bool(false) {
            self.failed_at = Some(now);
        }
        if payload["tool"] == "todo" && todos_done(&payload["output"]) {
            self.celebrated_at = Some(now);
        }
    }

    /// The turn ended (`turn.end`, or the client's `turn.finished`).
    pub fn turn_ended(&mut self, failed: bool, now: Instant) {
        self.busy = false;
        self.reasoning = false;
        self.tools_running = 0;
        self.waiting = 0;
        if failed {
            self.failed_at = Some(now);
        } else {
            self.finished_at = Some(now);
        }
    }

    /// A question the user must answer is shown or settled outside the
    /// trajectory (a server request withdrawn elsewhere).
    pub fn set_waiting(&mut self, count: u32) {
        self.waiting = count;
    }

    /// Celebrate (an achievement unlocked).
    pub fn celebrate(&mut self, now: Instant) {
        self.celebrated_at = Some(now);
    }

    /// The user sent an affectionate message: the heart shows.
    pub fn heart(&mut self, now: Instant) {
        self.heart_at = Some(now);
    }

    /// Whether the heart reaction shows at `now`.
    #[must_use]
    pub fn hearts(&self, now: Instant) -> bool {
        recent(self.heart_at, now)
    }

    /// The signals at `now`.
    #[must_use]
    pub fn activity(&self, now: Instant) -> Activity {
        Activity {
            failed: recent(self.failed_at, now),
            celebrate: recent(self.celebrated_at, now),
            awaiting_user: self.waiting > 0,
            tool_running: self.tools_running > 0,
            reasoning: self.reasoning,
            busy: self.busy,
            just_finished: recent(self.finished_at, now),
        }
    }

    /// The state at `now`.
    #[must_use]
    pub fn state(&self, now: Instant) -> PetState {
        derive(self.activity(now))
    }
}

fn recent(at: Option<Instant>, now: Instant) -> bool {
    at.is_some_and(|at| now.saturating_duration_since(at) < MOMENT)
}

/// Whether a todo tool's output holds at least one item, all completed or
/// cancelled.
fn todos_done(output: &Value) -> bool {
    let parsed;
    let reply = match output {
        Value::String(text) => {
            parsed = serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
            &parsed
        }
        other => other,
    };
    reply["todos"].as_array().is_some_and(|items| {
        !items.is_empty()
            && items
                .iter()
                .all(|t| matches!(t["status"].as_str(), Some("completed" | "cancelled")))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn precedence_follows_the_spec() {
        let all = Activity {
            failed: true,
            celebrate: true,
            awaiting_user: true,
            tool_running: true,
            reasoning: true,
            busy: true,
            just_finished: true,
        };
        assert_eq!(derive(all), PetState::Failed);
        let no_fail = Activity {
            failed: false,
            ..all
        };
        assert_eq!(derive(no_fail), PetState::Jump);
        let waiting = Activity {
            celebrate: false,
            ..no_fail
        };
        assert_eq!(derive(waiting), PetState::Waiting);
        let tool = Activity {
            awaiting_user: false,
            ..waiting
        };
        assert_eq!(derive(tool), PetState::Run);
        let reading = Activity {
            tool_running: false,
            ..tool
        };
        assert_eq!(derive(reading), PetState::Review);
        let busy = Activity {
            reasoning: false,
            ..reading
        };
        assert_eq!(derive(busy), PetState::Run);
        let done = Activity {
            busy: false,
            ..busy
        };
        assert_eq!(derive(done), PetState::Wave);
        assert_eq!(derive(Activity::default()), PetState::Idle);
    }

    /// Scenario: Approval pending.
    #[test]
    fn a_turn_waiting_on_an_approval_is_waiting() {
        let now = Instant::now();
        let mut pet = Tracker::default();
        pet.event("turn.start", &json!({}), now);
        pet.event("tool.call", &json!({"tool": "terminal"}), now);
        pet.event("approval.request", &json!({"request_id": "r"}), now);
        assert_eq!(pet.state(now), PetState::Waiting);
        pet.event("approval.decision", &json!({"request_id": "r"}), now);
        assert_eq!(pet.state(now), PetState::Run);
    }

    #[test]
    fn a_turn_moves_through_review_run_wave_and_idle() {
        let start = Instant::now();
        let mut pet = Tracker::default();
        pet.event("turn.start", &json!({}), start);
        pet.event("assistant.reasoning", &json!({"text": "hm"}), start);
        assert_eq!(pet.state(start), PetState::Review);
        pet.event("tool.call", &json!({"tool": "read_file"}), start);
        pet.event(
            "tool.result",
            &json!({"tool": "read_file", "ok": true}),
            start,
        );
        assert_eq!(pet.state(start), PetState::Run);
        pet.event("turn.end", &json!({"reason": "stop"}), start);
        assert_eq!(pet.state(start), PetState::Wave);
        assert_eq!(pet.state(start + MOMENT), PetState::Idle);
    }

    #[test]
    fn failures_and_completed_plans_are_momentary() {
        let now = Instant::now();
        let mut pet = Tracker::default();
        pet.event(
            "tool.result",
            &json!({"tool": "terminal", "ok": false}),
            now,
        );
        assert_eq!(pet.state(now), PetState::Failed);
        assert_eq!(pet.state(now + MOMENT), PetState::Idle);
        let todos = json!({"todos": [{"status": "completed"}, {"status": "cancelled"}]});
        pet.event(
            "tool.result",
            &json!({"tool": "todo", "ok": true, "output": todos.to_string()}),
            now + MOMENT,
        );
        assert_eq!(pet.state(now + MOMENT), PetState::Jump);
        let open = json!({"todos": [{"status": "completed"}, {"status": "pending"}]});
        let mut other = Tracker::default();
        other.event(
            "tool.result",
            &json!({"tool": "todo", "ok": true, "output": open}),
            now,
        );
        assert_eq!(other.state(now), PetState::Idle);
    }

    #[test]
    fn rows_follow_the_petdex_order() {
        assert_eq!(PetState::Idle.row(), 0);
        assert_eq!(PetState::Waiting.row(), 6);
        assert_eq!(PetState::Review.row(), 8);
        for state in PetState::ALL {
            assert_eq!(PetState::parse(state.as_str()), Some(state));
        }
    }
}
