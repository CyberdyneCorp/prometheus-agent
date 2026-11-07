//! Grace and missed occurrences (spec: scheduled-jobs, Grace and missed
//! occurrences): an occurrence fired at most 60 s after its instant is
//! `on_time`, within the job's grace `late`, beyond it `missed`. The grace
//! is the job's explicit `grace` (applied exactly, `0s` included), else
//! half the schedule period clamped to [2 min, 2 h], and 2 min for a
//! one-shot.

use jiff::Timestamp;

/// Seconds an occurrence may be late and still count as on time.
pub const ON_TIME_SECS: i64 = 60;
const MIN_DEFAULT: u64 = 120;
const MAX_DEFAULT: u64 = 7_200;

/// How late an occurrence fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lateness {
    /// Within 60 s.
    OnTime,
    /// Within the grace.
    Late,
    /// Beyond the grace.
    Missed,
}

impl Lateness {
    /// The stored literal.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OnTime => "on_time",
            Self::Late => "late",
            Self::Missed => "missed",
        }
    }
}

/// The grace of a job: `explicit`, else half of `period_secs` clamped to
/// [2 min, 2 h] (2 min without a period).
#[must_use]
pub fn grace_secs(explicit: Option<u64>, period_secs: Option<u64>) -> u64 {
    explicit.unwrap_or_else(|| {
        period_secs.map_or(MIN_DEFAULT, |p| (p / 2).clamp(MIN_DEFAULT, MAX_DEFAULT))
    })
}

/// The class of an occurrence at `scheduled` fired at `now`.
#[must_use]
pub fn classify(scheduled: Timestamp, now: Timestamp, grace_secs: u64) -> Lateness {
    let late = now.as_second() - scheduled.as_second();
    if late <= ON_TIME_SECS {
        Lateness::OnTime
    } else if late <= i64::try_from(grace_secs).unwrap_or(i64::MAX) {
        Lateness::Late
    } else {
        Lateness::Missed
    }
}

/// What to do with the occurrences due at a wake-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Run occurrence `scheduled`.
    Fire {
        /// The occurrence the run is recorded under.
        scheduled: Timestamp,
        /// Its class.
        lateness: Lateness,
        /// Occurrences it stands for (0 for an ordinary run).
        collapsed: u32,
    },
    /// Record missed occurrences as one `skipped_late` run.
    Skip {
        /// The newest missed occurrence (the record's key).
        scheduled: Timestamp,
        /// How many were missed.
        skipped: u32,
    },
}

/// The policy inputs of [`plan`].
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// The job's grace.
    pub grace_secs: u64,
    /// Whether the grace was given explicitly.
    pub explicit_grace: bool,
    /// `cron.catch_up_missed`.
    pub catch_up_missed: bool,
    /// The schedule recurs.
    pub recurring: bool,
}

/// The plan for `due` (ascending occurrences at or before `now`, not
/// empty): with no missed occurrence the oldest one fires; a backlog with
/// missed ones collapses into one catch-up run when the policy allows,
/// else the missed ones are recorded skipped (a missed one-shot too).
#[must_use]
pub fn plan(due: &[Timestamp], now: Timestamp, policy: Policy) -> Plan {
    let missed: Vec<&Timestamp> = due
        .iter()
        .filter(|at| classify(**at, now, policy.grace_secs) == Lateness::Missed)
        .collect();
    let Some(newest_missed) = missed.last() else {
        let scheduled = due[0];
        return Plan::Fire {
            scheduled,
            lateness: classify(scheduled, now, policy.grace_secs),
            collapsed: 0,
        };
    };
    if policy.recurring && policy.catch_up_missed && !policy.explicit_grace {
        return Plan::Fire {
            scheduled: *due.last().unwrap_or(newest_missed),
            lateness: Lateness::Missed,
            collapsed: u32::try_from(due.len()).unwrap_or(u32::MAX),
        };
    }
    Plan::Skip {
        scheduled: **newest_missed,
        skipped: u32::try_from(missed.len()).unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
mod tests {
    use jiff::{Timestamp, ToSpan as _};

    use super::{Lateness, Plan, Policy, classify, grace_secs, plan};

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn hourly(from: &str, count: i64) -> Vec<Timestamp> {
        (0..count)
            .map(|h| at(from).checked_add(h.hours()).unwrap())
            .collect()
    }

    #[test]
    fn grace_defaults_and_classes() {
        assert_eq!(grace_secs(None, Some(3_600)), 1_800);
        assert_eq!(grace_secs(None, Some(60)), 120);
        assert_eq!(grace_secs(None, Some(86_400)), 7_200);
        assert_eq!(grace_secs(None, None), 120);
        assert_eq!(grace_secs(Some(0), Some(3_600)), 0);
        let scheduled = at("2026-11-04T10:00:00Z");
        assert_eq!(
            classify(scheduled, at("2026-11-04T10:01:00Z"), 0),
            Lateness::OnTime
        );
        assert_eq!(
            classify(scheduled, at("2026-11-04T10:20:00Z"), 1_800),
            Lateness::Late
        );
        assert_eq!(
            classify(scheduled, at("2026-11-04T10:31:00Z"), 1_800),
            Lateness::Missed
        );
        assert_eq!(
            classify(scheduled, at("2026-11-04T10:02:00Z"), 0),
            Lateness::Missed
        );
    }

    /// Scenario: Default policy collapses the backlog.
    #[test]
    fn the_default_policy_collapses_a_backlog_into_one_run() {
        let due = hourly("2026-11-04T10:00:00Z", 5);
        let policy = Policy {
            grace_secs: 1_800,
            explicit_grace: false,
            catch_up_missed: true,
            recurring: true,
        };
        assert_eq!(
            plan(&due, at("2026-11-04T14:45:00Z"), policy),
            Plan::Fire {
                scheduled: due[4],
                lateness: Lateness::Missed,
                collapsed: 5
            }
        );
    }

    /// Scenario: Explicit zero grace means never catch up.
    #[test]
    fn an_explicit_zero_grace_records_the_backlog_skipped() {
        let due = hourly("2026-11-04T10:00:00Z", 3);
        let policy = Policy {
            grace_secs: 0,
            explicit_grace: true,
            catch_up_missed: true,
            recurring: true,
        };
        assert_eq!(
            plan(&due, at("2026-11-04T12:59:30Z"), policy),
            Plan::Skip {
                scheduled: due[2],
                skipped: 3
            }
        );
    }

    #[test]
    fn a_fresh_occurrence_fires_and_a_missed_one_shot_is_skipped() {
        let due = [at("2026-11-04T10:00:00Z")];
        let policy = Policy {
            grace_secs: 120,
            explicit_grace: false,
            catch_up_missed: true,
            recurring: false,
        };
        assert_eq!(
            plan(&due, at("2026-11-04T10:00:30Z"), policy),
            Plan::Fire {
                scheduled: due[0],
                lateness: Lateness::OnTime,
                collapsed: 0
            }
        );
        assert_eq!(
            plan(&due, at("2026-11-04T11:00:00Z"), policy),
            Plan::Skip {
                scheduled: due[0],
                skipped: 1
            }
        );
    }
}
