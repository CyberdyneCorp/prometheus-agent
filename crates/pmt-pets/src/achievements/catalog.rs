//! The achievements and what each counts (spec: pets, Achievements from
//! session history). Every badge reads one [`Metric`] of the profile's
//! history and has five tiers.

/// The tier names, lowest first.
pub const TIERS: [&str; 5] = ["copper", "silver", "gold", "diamond", "olympian"];

/// A number computed from the history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Metric {
    /// Most tool calls in one turn of one session.
    LongestToolChain,
    /// Distinct tools ever called.
    DistinctTools,
    /// Tool calls in all.
    ToolCalls,
    /// Failed tool calls later followed by a success of the same tool in
    /// the same session.
    Recoveries,
    /// Failed tool calls in all.
    ToolErrors,
    /// The longest run of consecutive local days with a session.
    DayStreak,
    /// Calls of the skill tools.
    SkillUses,
    /// Calls of the memory tool.
    MemoryUses,
    /// Distinct models of main-model calls.
    DistinctModels,
    /// Distinct providers of main-model calls.
    DistinctProviders,
    /// Sessions started 00:00-04:59 local time.
    NightSessions,
    /// Sessions started 05:00-07:59 local time.
    EarlySessions,
    /// Sessions started on a Saturday or Sunday.
    WeekendSessions,
    /// Most user and assistant messages in one session.
    LongestSession,
    /// Sessions in all.
    Sessions,
    /// Subagent delegations.
    Delegations,
    /// Calls of the web and browser tools.
    WebUses,
}

/// One achievement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Achievement {
    /// Its id (kebab-case).
    pub id: &'static str,
    /// Its name.
    pub name: &'static str,
    /// Exactly what it counts.
    pub description: &'static str,
    /// Its group.
    pub category: &'static str,
    /// The number it reads.
    pub metric: Metric,
    /// The thresholds of [`TIERS`].
    pub tiers: [u64; 5],
}

const fn badge(
    id: &'static str,
    name: &'static str,
    category: &'static str,
    description: &'static str,
    metric: Metric,
    tiers: [u64; 5],
) -> Achievement {
    Achievement {
        id,
        name,
        description,
        category,
        metric,
        tiers,
    }
}

/// Every achievement.
pub const ACHIEVEMENTS: &[Achievement] = &[
    badge(
        "chain-reaction",
        "Chain Reaction",
        "Tool chains",
        "The most tool calls the agent made in one turn of one session.",
        Metric::LongestToolChain,
        [10, 25, 50, 100, 250],
    ),
    badge(
        "toolsmith",
        "Toolsmith",
        "Tool chains",
        "Distinct tools the agent has called across all sessions.",
        Metric::DistinctTools,
        [5, 10, 20, 35, 50],
    ),
    badge(
        "busy-hands",
        "Busy Hands",
        "Tool chains",
        "Tool calls across all sessions.",
        Metric::ToolCalls,
        [100, 1_000, 5_000, 20_000, 100_000],
    ),
    badge(
        "bug-squasher",
        "Bug Squasher",
        "Debugging",
        "Failed tool calls that a later call of the same tool in the same session got right.",
        Metric::Recoveries,
        [5, 25, 100, 500, 2_000],
    ),
    badge(
        "red-text",
        "Red Text",
        "Debugging",
        "Tool calls that failed, across all sessions.",
        Metric::ToolErrors,
        [10, 100, 500, 2_000, 10_000],
    ),
    badge(
        "daily-streak",
        "Daily Streak",
        "Streaks",
        "The longest run of consecutive days (profile time zone) with at least one session.",
        Metric::DayStreak,
        [3, 7, 30, 100, 365],
    ),
    badge(
        "skillful",
        "Skillful",
        "Skills and memory",
        "Calls of the skill tools (skills_list, skill_view, skill_manage).",
        Metric::SkillUses,
        [1, 10, 50, 200, 1_000],
    ),
    badge(
        "elephant",
        "Elephant",
        "Skills and memory",
        "Calls of the memory tool.",
        Metric::MemoryUses,
        [1, 10, 50, 200, 1_000],
    ),
    badge(
        "model-hopper",
        "Model Hopper",
        "Models",
        "Distinct models that answered main-model calls.",
        Metric::DistinctModels,
        [2, 5, 10, 20, 40],
    ),
    badge(
        "provider-polyglot",
        "Provider Polyglot",
        "Models",
        "Distinct providers that answered main-model calls.",
        Metric::DistinctProviders,
        [2, 3, 5, 8, 12],
    ),
    badge(
        "night-owl",
        "Night Owl",
        "Timing",
        "Sessions started between 00:00 and 04:59 (profile time zone).",
        Metric::NightSessions,
        [1, 10, 50, 200, 1_000],
    ),
    badge(
        "early-bird",
        "Early Bird",
        "Timing",
        "Sessions started between 05:00 and 07:59 (profile time zone).",
        Metric::EarlySessions,
        [1, 10, 50, 200, 1_000],
    ),
    badge(
        "weekend-warrior",
        "Weekend Warrior",
        "Timing",
        "Sessions started on a Saturday or Sunday (profile time zone).",
        Metric::WeekendSessions,
        [1, 10, 50, 200, 1_000],
    ),
    badge(
        "marathon",
        "Marathon",
        "Sessions",
        "The most user and assistant messages in one session.",
        Metric::LongestSession,
        [50, 200, 500, 1_000, 5_000],
    ),
    badge(
        "regular",
        "Regular",
        "Sessions",
        "Sessions in all.",
        Metric::Sessions,
        [10, 100, 500, 2_000, 10_000],
    ),
    badge(
        "delegator",
        "Delegator",
        "Sessions",
        "Subagents started with delegate_task.",
        Metric::Delegations,
        [1, 10, 50, 200, 1_000],
    ),
    badge(
        "researcher",
        "Researcher",
        "Tool chains",
        "Calls of the web and browser tools (web_search, web_extract, browser_*).",
        Metric::WebUses,
        [10, 100, 500, 2_000, 10_000],
    ),
];

/// The achievement `id`.
#[must_use]
pub fn find(id: &str) -> Option<&'static Achievement> {
    ACHIEVEMENTS.iter().find(|a| a.id == id)
}

/// The index of the highest tier `value` reaches.
#[must_use]
pub fn tier_of(achievement: &Achievement, value: u64) -> Option<usize> {
    achievement.tiers.iter().rposition(|&t| value >= t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_tiers_rise() {
        let mut ids: Vec<&str> = ACHIEVEMENTS.iter().map(|a| a.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ACHIEVEMENTS.len());
        for a in ACHIEVEMENTS {
            assert!(a.tiers.windows(2).all(|w| w[0] < w[1]), "{}", a.id);
        }
        let owl = find("night-owl").unwrap();
        assert_eq!(tier_of(owl, 0), None);
        assert_eq!(tier_of(owl, 10).map(|t| TIERS[t]), Some("silver"));
    }
}
