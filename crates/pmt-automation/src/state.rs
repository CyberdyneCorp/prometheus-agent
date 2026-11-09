//! The persisted automation state of one session (spec: session-automation,
//! Persisted per-session automation state). Each kind is one row of the
//! profile's `automation` table, its `state` column the JSON of the
//! structs here; the host reads it at every boundary, so a resumed or
//! restarted session continues exactly where it was.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::contract::Contract;

/// The three automation kinds (`kind` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `/goal`.
    Goal,
    /// `/loop`.
    Loop,
    /// `/heartbeat`.
    Heartbeat,
}

impl Kind {
    /// Every kind, in arbitration order.
    pub const ALL: [Self; 3] = [Self::Goal, Self::Loop, Self::Heartbeat];

    /// The stored spelling (also the `origin` of an automated turn).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Goal => "goal",
            Self::Loop => "loop",
            Self::Heartbeat => "heartbeat",
        }
    }

    /// The kind spelled `text`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == text)
    }
}

/// Where a messaging-owned automation posts (spec: Heartbeat ownership,
/// recovery and silence on the gateway): the conversation it was set in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// Platform name.
    pub platform: String,
    /// Chat id.
    pub chat_id: String,
    /// Thread or topic id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Who set it (the automated turns run as them).
    #[serde(default)]
    pub sender_id: String,
}

/// The configured bounds (`[goals]`, `[loops]`, `[heartbeat]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `goals.max_turns`.
    pub goal_max_turns: u32,
    /// `goals.gate_max_retries`.
    pub gate_max_retries: u32,
    /// `goals.max_wait_secs`.
    pub max_wait_secs: u64,
    /// `loops.min_interval_secs`.
    pub loop_min_interval_secs: u64,
    /// `loops.max_ticks` (`0` = unlimited).
    pub loop_max_ticks: u32,
    /// `loops.self_paced_floor_secs`.
    pub self_paced_floor_secs: u64,
    /// `loops.self_paced_ceiling_secs`.
    pub self_paced_ceiling_secs: u64,
    /// `heartbeat.min_interval_secs`.
    pub heartbeat_min_interval_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            goal_max_turns: 20,
            gate_max_retries: 3,
            max_wait_secs: 1_800,
            loop_min_interval_secs: 30,
            loop_max_ticks: 100,
            self_paced_floor_secs: 60,
            self_paced_ceiling_secs: 900,
            heartbeat_min_interval_secs: 60,
        }
    }
}

/// A goal's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// The judge runs after every turn.
    Active,
    /// Waiting for `/goal resume`.
    Paused,
    /// The judge ruled it achieved.
    Done,
}

impl GoalStatus {
    /// The spelling `/goal status` shows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Done => "done",
        }
    }
}

/// A shell-command quality gate (spec: Quality gates).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    /// The command line.
    pub command: String,
    /// Consecutive failed boundaries.
    #[serde(default)]
    pub failures: u32,
}

/// What a parked goal waits for (spec: Goal wait barriers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Barrier {
    /// A local process exits.
    Pid {
        /// Its pid.
        pid: u32,
        /// Its start time (a reused pid is not the one awaited).
        #[serde(default)]
        start_time: u64,
        /// Released at this time at the latest (ms since the epoch).
        until_ms: i64,
        /// Why.
        reason: String,
    },
    /// A background process of the session exits or matches its watch.
    Process {
        /// Its process id (`proc_…`).
        id: String,
        /// Watch matches seen when parking (a new match releases).
        #[serde(default)]
        matches: u32,
        /// Released at this time at the latest.
        until_ms: i64,
        /// Why.
        reason: String,
    },
    /// A fixed time passes.
    Seconds {
        /// When.
        until_ms: i64,
        /// Why.
        reason: String,
    },
}

impl Barrier {
    /// The latest release time.
    #[must_use]
    pub fn until_ms(&self) -> i64 {
        match self {
            Self::Pid { until_ms, .. }
            | Self::Process { until_ms, .. }
            | Self::Seconds { until_ms, .. } => *until_ms,
        }
    }

    /// What `/goal status` says the goal is parked on.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Pid { pid, reason, .. } => with_reason(&format!("pid {pid}"), reason),
            Self::Process { id, reason, .. } => with_reason(&format!("process {id}"), reason),
            Self::Seconds { until_ms, reason } => {
                with_reason(&format!("a timer until {}", rfc3339(*until_ms)), reason)
            }
        }
    }
}

fn with_reason(what: &str, reason: &str) -> String {
    if reason.is_empty() {
        what.to_owned()
    } else {
        format!("{what} ({reason})")
    }
}

/// `ms` since the epoch as an RFC 3339 UTC time (seconds precision).
#[must_use]
pub fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1_000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 as a civil date (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The standing goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    /// The headline.
    pub text: String,
    /// The optional completion contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<Contract>,
    /// Lifecycle.
    pub status: GoalStatus,
    /// Continuation turns used since it was set or resumed.
    #[serde(default)]
    pub turns_used: u32,
    /// `/subgoal` criteria, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subgoals: Vec<String>,
    /// Quality gates, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<Gate>,
    /// The wait barrier it is parked on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barrier: Option<Barrier>,
    /// The judge's (or pause's) last reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Its first turn has not run yet (it starts at the next idle
    /// boundary, right after `/goal` set it).
    #[serde(default)]
    pub kickoff: bool,
    /// The conversation it posts to (messaging owners).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<Route>,
}

impl Goal {
    /// A new active goal (replacing a goal drops its subgoals, gates and
    /// barrier: they are not carried over).
    #[must_use]
    pub fn new(text: String, contract: Option<Contract>) -> Self {
        Self {
            text,
            contract,
            status: GoalStatus::Active,
            turns_used: 0,
            subgoals: Vec::new(),
            gates: Vec::new(),
            barrier: None,
            reason: None,
            kickoff: true,
            route: None,
        }
    }

    /// Active and not parked: the judge runs after each turn.
    #[must_use]
    pub fn drives(&self) -> bool {
        self.status == GoalStatus::Active && self.barrier.is_none()
    }

    /// The first turn of a new goal: the goal and its contract.
    #[must_use]
    pub fn kickoff_prompt(&self) -> String {
        match self.contract.as_ref().filter(|c| !c.is_empty()) {
            Some(contract) => format!("{}\n\n{}", self.text, contract.render()),
            None => self.text.clone(),
        }
    }

    /// The `/goal status` text.
    #[must_use]
    pub fn status_text(&self, max_turns: u32) -> String {
        let mut text = format!(
            "Goal: {}\nstatus: {} · {}/{} turns used",
            self.text,
            self.status.as_str(),
            self.turns_used,
            max_turns
        );
        if let Some(barrier) = &self.barrier {
            let _ = write!(text, "\nparked on {}", barrier.describe());
        }
        if let Some(reason) = &self.reason {
            let _ = write!(text, "\nlast reason: {reason}");
        }
        if !self.subgoals.is_empty() {
            let _ = write!(text, "\nsubgoals: {}", self.subgoals.len());
        }
        if !self.gates.is_empty() {
            let _ = write!(text, "\ngates: {}", self.gates.len());
        }
        text
    }
}

/// Whether a loop or heartbeat fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Fires when due.
    Active,
    /// Waits for `resume`.
    Paused,
}

impl RunStatus {
    /// Its spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
        }
    }
}

/// A loop's cadence (spec: Loop command and cadence).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cadence {
    /// A given interval.
    Fixed {
        /// Seconds between wakeups.
        every_secs: u64,
    },
    /// No interval: backs off while the replies do not change.
    SelfPaced {
        /// The current wait.
        wait_secs: u64,
        /// Digest of the last reply (timestamps ignored).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        digest: Option<String>,
    },
}

/// The session's `/loop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loop {
    /// The prompt or slash command of each wakeup.
    pub prompt: String,
    /// Timing.
    pub cadence: Cadence,
    /// `--times N`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u32>,
    /// `--until <condition>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Wakeups fired.
    #[serde(default)]
    pub fired: u32,
    /// Active or paused.
    pub status: RunStatus,
    /// The next wakeup (ms since the epoch); due at the next idle boundary
    /// once passed.
    pub next_fire_ms: i64,
    /// Why it paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The conversation it posts to (messaging owners).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<Route>,
}

impl Loop {
    /// The live row: `loop · every 5m · 2 runs · next in 4m`.
    #[must_use]
    pub fn row(&self, now_ms: i64) -> String {
        let cadence = match &self.cadence {
            Cadence::Fixed { every_secs } => {
                format!("every {}", crate::duration::format(*every_secs))
            }
            Cadence::SelfPaced { .. } => "self-paced".to_owned(),
        };
        let runs = if self.fired == 1 { "run" } else { "runs" };
        let tail = match self.status {
            RunStatus::Paused => "paused".to_owned(),
            RunStatus::Active => next_in(self.next_fire_ms, now_ms),
        };
        format!("loop · {cadence} · {} {runs} · {tail}", self.fired)
    }
}

/// The session's `/heartbeat`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// What each fire asks.
    pub prompt: String,
    /// Seconds between fires.
    pub every_secs: u64,
    /// Active or paused.
    pub status: RunStatus,
    /// The next fire (ms since the epoch).
    pub next_fire_ms: i64,
    /// Fires so far.
    #[serde(default)]
    pub fired: u32,
    /// The conversation it posts to (messaging owners).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<Route>,
}

impl Heartbeat {
    /// The live row: `heartbeat · every 10m · next in 3m`.
    #[must_use]
    pub fn row(&self, now_ms: i64) -> String {
        let tail = match self.status {
            RunStatus::Paused => "paused".to_owned(),
            RunStatus::Active => next_in(self.next_fire_ms, now_ms),
        };
        format!(
            "heartbeat · every {} · {tail}",
            crate::duration::format(self.every_secs)
        )
    }
}

/// `next in 4m` (rounded up to whole seconds; `next now` once due).
#[must_use]
pub fn next_in(at_ms: i64, now_ms: i64) -> String {
    let left = at_ms.saturating_sub(now_ms);
    if left <= 0 {
        return "next now".to_owned();
    }
    let secs = u64::try_from((left + 999) / 1_000).unwrap_or(0);
    format!("next in {}", crate::duration::format(secs))
}

/// Everything automated in one session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Automation {
    /// The goal.
    pub goal: Option<Goal>,
    /// The loop.
    pub looping: Option<Loop>,
    /// The heartbeat.
    pub heartbeat: Option<Heartbeat>,
}

impl Automation {
    /// Nothing is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.goal.is_none() && self.looping.is_none() && self.heartbeat.is_none()
    }

    /// An active loop or heartbeat keeps the session from being reaped.
    #[must_use]
    pub fn keeps_alive(&self) -> bool {
        self.looping
            .as_ref()
            .is_some_and(|l| l.status == RunStatus::Active)
            || self
                .heartbeat
                .as_ref()
                .is_some_and(|h| h.status == RunStatus::Active)
    }

    /// The earliest time something may fire: an active loop or heartbeat,
    /// or a parked goal's barrier deadline.
    #[must_use]
    pub fn next_fire_ms(&self) -> Option<i64> {
        let looping = self
            .looping
            .as_ref()
            .filter(|l| l.status == RunStatus::Active)
            .map(|l| l.next_fire_ms);
        let heartbeat = self
            .heartbeat
            .as_ref()
            .filter(|h| h.status == RunStatus::Active)
            .map(|h| h.next_fire_ms);
        let barrier = self
            .goal
            .as_ref()
            .filter(|g| g.status == GoalStatus::Active)
            .and_then(|g| g.barrier.as_ref())
            .map(Barrier::until_ms);
        [looping, heartbeat, barrier].into_iter().flatten().min()
    }

    /// The live rows of every surface (`/agents`, the CLI and TUI rows).
    #[must_use]
    pub fn rows(&self, now_ms: i64, limits: &Limits) -> Vec<String> {
        let mut rows = Vec::new();
        if let Some(goal) = &self.goal {
            let parked = goal
                .barrier
                .as_ref()
                .map_or(String::new(), |b| format!(" · parked on {}", b.describe()));
            rows.push(format!(
                "goal · {} · {}/{} turns{parked} · {}",
                goal.status.as_str(),
                goal.turns_used,
                limits.goal_max_turns,
                goal.text
            ));
        }
        if let Some(looping) = &self.looping {
            rows.push(looping.row(now_ms));
        }
        if let Some(heartbeat) = &self.heartbeat {
            rows.push(heartbeat.row(now_ms));
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_render_in_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_767_225_600_000), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn the_loop_row_names_cadence_runs_and_next_fire() {
        let looping = Loop {
            prompt: "check".into(),
            cadence: Cadence::Fixed { every_secs: 300 },
            times: None,
            until: None,
            fired: 2,
            status: RunStatus::Active,
            next_fire_ms: 240_000,
            reason: None,
            route: None,
        };
        assert_eq!(looping.row(0), "loop · every 5m · 2 runs · next in 4m");
    }

    #[test]
    fn the_goal_state_round_trips_as_json() {
        let mut goal = Goal::new("ship it".into(), None);
        goal.barrier = Some(Barrier::Pid {
            pid: 4242,
            start_time: 1,
            until_ms: 10,
            reason: "CI running".into(),
        });
        let json = serde_json::to_string(&goal).unwrap();
        let back: Goal = serde_json::from_str(&json).unwrap();
        assert_eq!(back, goal);
        assert!(!back.drives());
        assert!(
            back.status_text(20)
                .contains("parked on pid 4242 (CI running)")
        );
    }
}
