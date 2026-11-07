//! The `[goals]`, `[loops]` and `[heartbeat]` sections (spec:
//! session-automation, Automation configuration). Out-of-range values fail
//! validation instead of being clamped.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[goals]` section: the `/goal` judge loop's bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct GoalsConfig {
    /// Continuation turns a goal may use before it pauses.
    #[schemars(range(min = 1, max = 1000))]
    pub max_turns: u32,
    /// Seconds one quality gate command may run.
    #[schemars(range(min = 1, max = 86_400))]
    pub gate_timeout_secs: u64,
    /// Consecutive failures of a quality gate after which the goal pauses.
    #[schemars(range(min = 1, max = 100))]
    pub gate_max_retries: u32,
    /// Longest wait of a goal parked on a process (seconds).
    #[schemars(range(min = 1, max = 86_400))]
    pub max_wait_secs: u64,
}

impl Default for GoalsConfig {
    fn default() -> Self {
        Self {
            max_turns: 20,
            gate_timeout_secs: 300,
            gate_max_retries: 3,
            max_wait_secs: 1_800,
        }
    }
}

/// The `[loops]` section: `/loop` cadence bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct LoopsConfig {
    /// Shortest fixed interval a loop may use (seconds).
    #[schemars(range(min = 1, max = 86_400))]
    pub min_interval_secs: u64,
    /// Wakeups after which a loop pauses (`0` = unlimited).
    #[schemars(range(min = 0, max = 100_000))]
    pub max_ticks: u32,
    /// First wait of a self-paced loop (seconds).
    #[schemars(range(min = 1, max = 86_400))]
    pub self_paced_floor_secs: u64,
    /// Longest wait of a self-paced loop (seconds).
    #[schemars(range(min = 1, max = 86_400))]
    pub self_paced_ceiling_secs: u64,
}

impl Default for LoopsConfig {
    fn default() -> Self {
        Self {
            min_interval_secs: 30,
            max_ticks: 100,
            self_paced_floor_secs: 60,
            self_paced_ceiling_secs: 900,
        }
    }
}

/// The `[heartbeat]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct HeartbeatConfig {
    /// Shortest `/heartbeat every` interval (seconds).
    #[schemars(range(min = 1, max = 86_400))]
    pub min_interval_secs: u64,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            min_interval_secs: 60,
        }
    }
}
