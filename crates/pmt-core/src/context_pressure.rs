//! The context pressure indicator (spec: context-compression, Compaction
//! progress notices and context pressure gauge): how close the session's
//! context is to its compaction trigger. Surfaces show it (the CLI and the
//! TUI as a gauge line, messaging as one notice per level crossing); it is
//! never added to the model's context. It also words the compaction
//! lifecycle notices (`compression.started|committed|failed`) every
//! surface shows.

use serde_json::Value;

/// The info level starts here (percent of the compaction trigger).
pub const INFO_PERCENT: u64 = 60;
/// The warning level starts here.
pub const WARNING_PERCENT: u64 = 85;
/// Cells of the gauge bar.
const CELLS: u64 = 20;

/// How pressing the context is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// From 60% of the trigger.
    Info,
    /// From 85% of the trigger.
    Warning,
}

/// `tokens` as a percentage of `trigger` (0 when the trigger is unknown).
#[must_use]
pub fn percent(tokens: u64, trigger: u64) -> u64 {
    if trigger == 0 {
        return 0;
    }
    tokens.saturating_mul(100) / trigger
}

/// The level of `percent`; `None` below 60%.
#[must_use]
pub const fn level(percent: u64) -> Option<Level> {
    if percent >= WARNING_PERCENT {
        Some(Level::Warning)
    } else if percent >= INFO_PERCENT {
        Some(Level::Info)
    } else {
        None
    }
}

/// The CLI/TUI gauge line: `context ████████████░░░░░░░░ 62% to compaction`.
#[must_use]
pub fn gauge(percent: u64) -> String {
    let filled = percent.min(100) * CELLS / 100;
    let bar: String = (0..CELLS)
        .map(|i| if i < filled { '█' } else { '░' })
        .collect();
    format!("context {bar} {percent}% to compaction")
}

/// The plain-text notice a messaging chat gets when the context crosses
/// into `level`.
#[must_use]
pub fn notice(level: Level, percent: u64) -> String {
    match level {
        Level::Info => format!("Context is at {percent}% of the compaction trigger."),
        Level::Warning => format!(
            "Context is at {percent}% of the compaction trigger; older messages will be summarized soon."
        ),
    }
}

/// A compaction lifecycle notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// What the user reads.
    pub text: String,
    /// A routine status (start, retry, completion): messaging shows it
    /// only with `compression.progress_notices`. Failures are not routine.
    pub routine: bool,
}

/// The notice of trajectory event `kind` with `payload`, when it is a
/// compaction lifecycle event a user should see. Manual `/compress` runs
/// get none for their start and completion: the command answers itself.
#[must_use]
pub fn compaction_notice(kind: &str, payload: &Value) -> Option<Notice> {
    let trigger = payload["trigger"].as_str().unwrap_or_default();
    let routine = |text: String| {
        Some(Notice {
            text,
            routine: true,
        })
    };
    let manual = trigger == "manual";
    match kind {
        "compression.started" | "compression.committed" if manual => None,
        "compression.started" if payload["attempt"].as_u64().unwrap_or(1) > 1 => {
            routine("Rewriting the compaction summary: the first one missed details.".to_owned())
        }
        "compression.started" if trigger == "idle" => {
            routine("Compacting the context after a long pause...".to_owned())
        }
        "compression.started" => routine(format!("Compacting the context ({trigger})...")),
        "compression.committed" => {
            let before = payload["tokens_before"].as_u64().unwrap_or(0);
            let after = payload["tokens_after"].as_u64().unwrap_or(0);
            routine(format!(
                "Context compacted ({trigger}): {before} -> {after} tokens."
            ))
        }
        "compression.failed" => {
            let reason = payload["reason"].as_str().unwrap_or("unknown");
            let text = match payload["message"].as_str() {
                Some(message) => format!("Context compaction failed ({reason}): {message}"),
                None => format!("Context compaction failed ({reason})."),
            };
            Some(Notice {
                text,
                routine: false,
            })
        }
        _ => None,
    }
}

/// The gauge of a `turn.end` payload: `(percent, level)` when it carries
/// the context size and the trigger.
#[must_use]
pub fn of_turn_end(payload: &Value) -> Option<(u64, Option<Level>)> {
    let tokens = payload["context_tokens"].as_u64()?;
    let trigger = payload["compaction_trigger_tokens"].as_u64()?;
    let percent = percent(tokens, trigger);
    Some((percent, level(percent)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gauge_fills_one_cell_per_five_percent() {
        assert_eq!(gauge(62), "context ████████████░░░░░░░░ 62% to compaction");
        assert_eq!(gauge(130).matches('█').count(), 20);
    }

    #[test]
    fn manual_runs_and_unknown_events_get_no_notice() {
        let manual = serde_json::json!({"trigger": "manual", "attempt": 1});
        assert_eq!(compaction_notice("compression.started", &manual), None);
        assert_eq!(compaction_notice("compression.committed", &manual), None);
        let idle = serde_json::json!({"trigger": "idle", "tokens_before": 9, "tokens_after": 3});
        let done = compaction_notice("compression.committed", &idle).unwrap();
        assert!(done.routine && done.text.contains("9 -> 3"), "{done:?}");
        let failed = serde_json::json!({"reason": "aux_failed"});
        assert!(
            !compaction_notice("compression.failed", &failed)
                .unwrap()
                .routine
        );
        assert_eq!(compaction_notice("turn.end", &failed), None);
    }

    #[test]
    fn levels_start_at_60_and_85() {
        assert_eq!(level(59), None);
        assert_eq!(level(percent(62, 100)), Some(Level::Info));
        assert_eq!(level(85), Some(Level::Warning));
        assert_eq!(percent(5, 0), 0);
    }
}
