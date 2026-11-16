//! Automatic jobs after failed or long sessions (spec: harness-evolution,
//! Automatic triggers after failed or long tasks).

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_store::{EndReason, LineageKind, Role, Source, Store, Visibility};

use super::{JobRecord, Jobs, Profile, Request, TaskInput, Trigger};

/// What a finished session amounts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    /// Its surface.
    pub source: Source,
    /// Why it ended.
    pub reason: EndReason,
    /// Tool calls it made.
    pub tool_calls: u64,
    /// Agent iterations (model replies) it took.
    pub iterations: u64,
    /// A user's own session: not hidden and not a delegated child.
    pub user_task: bool,
}

/// Whether a finished session queues a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Queue one; the reason is `failure` or `long_task`.
    Queue(&'static str),
    /// Do not; the reason is logged when it is worth telling.
    Skip(&'static str),
}

/// The reason logged when the daily cap stops a job.
pub const DAILY_CAP: &str = "daily auto-evolve cap reached";

/// Decides for `ended`, with `queued_today` automatic jobs already queued.
#[must_use]
pub fn decide(config: &pmt_config::Config, ended: &Ended, queued_today: usize) -> Decision {
    let auto = &config.evolve.auto;
    if !config.evolve.enabled || !auto.enabled {
        return Decision::Skip("automatic evolution is off");
    }
    // Only a user's finished task evolves the harness: a rollout never
    // evolves itself, and an eval task (or a hidden or delegated session)
    // is not the user's task; evolving on benchmark prompts would tune
    // the harness to the very suite that measures it.
    if ended.source == Source::Harness {
        return Decision::Skip("evolution session");
    }
    if ended.source == Source::Eval || !ended.user_task {
        return Decision::Skip("not a user task");
    }
    let failed =
        matches!(ended.reason, EndReason::Error | EndReason::Interrupted) && ended.tool_calls >= 1;
    let reason = if auto.on_failure && failed {
        "failure"
    } else if auto.on_long_task && ended.iterations >= u64::from(auto.min_iterations) {
        "long_task"
    } else {
        return Decision::Skip("session does not qualify");
    };
    if queued_today >= usize::try_from(auto.max_jobs_per_day).unwrap_or(usize::MAX) {
        return Decision::Skip(DAILY_CAP);
    }
    Decision::Queue(reason)
}

/// Automatic jobs queued on the current UTC day.
///
/// # Errors
///
/// `harness.io` reading the jobs.
pub fn queued_today(jobs: &Jobs) -> Result<usize, Error> {
    let today = jiff::Timestamp::now().to_string();
    let day = &today[..10];
    Ok(jobs
        .list()?
        .iter()
        .filter(|j| j.trigger == Trigger::Auto && j.created_at.starts_with(day))
        .count())
}

/// Runs the automatic trigger for session `id` that just ended with
/// `reason`: the queued job, if any (the caller notifies the user).
///
/// # Errors
///
/// Store and file errors; capture failures.
pub fn after_session(
    profile: &Profile<'_>,
    store: &Store,
    id: SessionId,
    reason: EndReason,
) -> Result<Option<JobRecord>, Error> {
    let evolve = &profile.config.evolve;
    if !evolve.enabled || !evolve.auto.enabled {
        return Ok(None);
    }
    let session = store.session(id)?;
    let iterations = store
        .messages(id, Visibility::All)?
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count();
    let ended = Ended {
        source: session.source,
        reason,
        tool_calls: session.tool_call_count,
        iterations: u64::try_from(iterations).unwrap_or(u64::MAX),
        user_task: !session.hidden && session.lineage_kind != Some(LineageKind::Delegate),
    };
    let jobs = Jobs::of(profile.dir);
    match decide(profile.config, &ended, queued_today(&jobs)?) {
        Decision::Queue(why) => {
            let job = super::queue(
                profile,
                &Request::new(Trigger::Auto, TaskInput::Session(id)),
            )?;
            tracing::info!(session = %id, job = %job.id, reason = why, "automatic evolution job queued");
            Ok(Some(job))
        }
        Decision::Skip(why) => {
            if why == DAILY_CAP {
                tracing::info!(session = %id, reason = why, "no automatic evolution job queued");
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> pmt_config::Config {
        let mut config = pmt_config::Config::default();
        config.evolve.enabled = true;
        config.evolve.auto.enabled = true;
        config
    }

    fn ended(reason: EndReason, tool_calls: u64, iterations: u64) -> Ended {
        Ended {
            source: Source::Cli,
            reason,
            tool_calls,
            iterations,
            user_task: true,
        }
    }

    #[test]
    fn failed_and_long_sessions_qualify_and_others_do_not() {
        let config = config();
        let failed = ended(EndReason::Error, 1, 3);
        assert_eq!(decide(&config, &failed, 0), Decision::Queue("failure"));
        let interrupted = ended(EndReason::Interrupted, 2, 3);
        assert_eq!(decide(&config, &interrupted, 0), Decision::Queue("failure"));
        let no_tools = ended(EndReason::Error, 0, 3);
        assert!(matches!(decide(&config, &no_tools, 0), Decision::Skip(_)));
        let long = ended(EndReason::UserExit, 0, 40);
        assert_eq!(decide(&config, &long, 0), Decision::Queue("long_task"));
        let short = ended(EndReason::UserExit, 5, 39);
        assert!(matches!(decide(&config, &short, 0), Decision::Skip(_)));
        let rollout = Ended {
            source: Source::Harness,
            ..failed
        };
        assert_eq!(
            decide(&config, &rollout, 0),
            Decision::Skip("evolution session")
        );
        for not_a_task in [
            Ended {
                source: Source::Eval,
                ..failed
            },
            Ended {
                user_task: false,
                ..failed
            },
        ] {
            assert_eq!(
                decide(&config, &not_a_task, 0),
                Decision::Skip("not a user task")
            );
        }
        assert!(matches!(
            decide(&pmt_config::Config::default(), &failed, 0),
            Decision::Skip(_)
        ));
    }

    /// Scenario: Daily cap (the decision; the INFO record is
    /// `after_session`'s, tested end to end).
    #[test]
    fn a_fourth_qualifying_session_is_stopped_by_the_daily_cap() {
        let config = config();
        let failed = ended(EndReason::Error, 1, 3);
        assert_eq!(decide(&config, &failed, 2), Decision::Queue("failure"));
        assert_eq!(decide(&config, &failed, 3), Decision::Skip(DAILY_CAP));
    }
}
