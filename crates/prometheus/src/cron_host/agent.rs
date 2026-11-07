//! The agent side of a run (spec: scheduled-jobs, Fresh isolated agent
//! session per run; Memory policy for unattended runs; Execution
//! concurrency and timeouts): a new `source = cron` session through the
//! session factory, with no history, the job's skills preloaded, its
//! toolsets and memory mode, approvals by `approvals.cron_mode`, and
//! interrupted when idle (no tokens, no events) or past its wall limit.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pmt_agent::turn::events::EventSink;
use pmt_agent::turn::model::{BoxFuture, DeltaSink};
use pmt_agent::turn::{ExitReason, Issuer, TurnInput, TurnOptions};
use pmt_config::ProfileScope;
use pmt_core::cancel::CancelToken;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::secret::SecretString;
use pmt_cron::exec::{AgentEnd, AgentOutcome, AgentRequest};
use pmt_cron::job::{Job, MemoryMode};
use pmt_obs::trajectory::writer::Emit;
use pmt_skills::secrets::SecretPrompt;
use pmt_store::{EndReason, Store, UsageScope};

use crate::chat::session::cron::CronRun;
use crate::chat::session::model::RouteFlags;
use crate::chat::session::{Clients, Factory, Options};

/// How often the time limits are checked.
const WATCH: Duration = Duration::from_millis(200);
/// The source tag of `cron replay --live` sessions.
pub(crate) const REPLAY_TAG: &str = "cron_replay";
/// How long an interrupted turn may take to wind down.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// The profile as a run of `job` sees it: no background memory review,
/// and with `memory = off` no memory at all.
fn run_scope(scope: &ProfileScope, job: &Job) -> ProfileScope {
    let memory = job.spec.memory;
    let reasoning = job
        .spec
        .reasoning_effort
        .as_deref()
        .and_then(|level| serde_json::from_value(serde_json::json!(level)).ok());
    scope.patched(move |config| {
        config.memory.nudge_interval = 0;
        if memory == MemoryMode::Off {
            config.memory.memory_enabled = false;
            config.memory.user_profile_enabled = false;
        }
        if reasoning.is_some() {
            config.agent.reasoning_effort = reasoning;
        }
    })
}

/// The run's model (spec: Model pinning and cron fleet default): the
/// job's pin, else `cron.model` / `cron.model_provider`, else the
/// profile's main model at fire time.
fn flags<'a>(job: &'a Job, config: &'a pmt_config::schema::CronConfig) -> RouteFlags<'a> {
    if job.spec.pinned() {
        return RouteFlags {
            provider: job.spec.provider.as_deref(),
            model: job.spec.model.as_deref(),
            mock_script: None,
        };
    }
    RouteFlags {
        provider: config.model_provider.as_deref(),
        model: config.model.as_deref(),
        mock_script: None,
    }
}

fn factory(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    store: Arc<Store>,
    job: &Job,
    replay: bool,
    notepad: Option<&std::path::Path>,
) -> Result<Factory, Error> {
    let config = &scope.config().cron;
    let workspace = super::script::cwd(scope, job);
    std::fs::create_dir_all(&workspace).map_err(|e| {
        Error::new(
            &pmt_core::error::codes::OUTPUT_IO,
            format!("cannot create {}: {e}", workspace.display()),
        )
    })?;
    let options = Options {
        interactive: false,
        cwd: workspace,
        cwd_fallback: false,
        skills: job.spec.skills.clone(),
        // A job with a workdir loads its project context files; a
        // detached one loads none.
        no_context_files: job.spec.workdir.is_none(),
        source_tag: replay.then(|| REPLAY_TAG.to_owned()),
        cron: Some(CronRun {
            job_id: job.id().to_owned(),
            toolsets: job
                .spec
                .toolsets
                .clone()
                .or_else(|| config.default_toolsets.clone()),
            memory_write: job.spec.memory == MemoryMode::ReadWrite,
            allow_scheduling: config.allow_agent_scheduling,
            notepad: notepad.map(std::path::Path::to_path_buf),
        }),
        ..Options::default()
    };
    Factory::with_store(
        run_scope(scope, job),
        store,
        launch,
        &flags(job, config),
        None,
        options,
    )
}

/// Whether the job's provider credential resolves (no request is sent);
/// `Err` names what is missing.
pub(super) async fn credential(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    job: &Job,
) -> Result<(), String> {
    let store = Arc::new(Store::open(scope).map_err(|e| e.message().to_owned())?);
    let factory =
        factory(scope, launch, store, job, false, None).map_err(|e| e.message().to_owned())?;
    match factory
        .models
        .primary(&flags(job, &scope.config().cron))
        .await
    {
        Ok(_) => Ok(()),
        // The credential is configured, only benched: a closed usage
        // window, which the run itself turns into a hold.
        Err(err) if pmt_providers::resolve::bench_end(&err).is_some() => Ok(()),
        Err(err) => Err(err.message().to_owned()),
    }
}

/// When the run last showed activity, in ms since it started.
struct Activity {
    started: Instant,
    last: AtomicU64,
}

impl Activity {
    fn touch(&self) {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last.store(now, Ordering::Relaxed);
    }

    fn idle(&self) -> Duration {
        let last = self.last.load(Ordering::Relaxed);
        self.started
            .elapsed()
            .saturating_sub(Duration::from_millis(last))
    }
}

impl EventSink for Activity {
    fn emit(&self, _: Emit) -> BoxFuture<'_, Result<(), Error>> {
        self.touch();
        Box::pin(async { Ok(()) })
    }
}

impl DeltaSink for Activity {
    fn text(&self, _: &str) {
        self.touch();
    }
}

/// Nobody answers a scheduled run's secret prompts.
struct NoPrompt;

impl SecretPrompt for NoPrompt {
    fn ask(&self, _: &str, _: &str, _: Option<&str>) -> Option<SecretString> {
        None
    }
}

/// Runs the agent of `request`.
pub(super) async fn run(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    store: Arc<Store>,
    request: AgentRequest,
) -> AgentOutcome {
    let mut outcome = AgentOutcome {
        session_id: None,
        end: AgentEnd::Failed(String::new()),
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        reached_model: false,
    };
    let mut closed = None;
    match Box::pin(session(scope, launch, &store, &request, &mut outcome)).await {
        Ok(end) => outcome.end = end,
        Err(err) => {
            // With a fallback chain configured, the job is not held: a
            // fallback may answer the next run.
            closed = pmt_providers::resolve::bench_end(&err)
                .filter(|_| scope.config().fallback.chain.is_empty())
                .map(jiff::Timestamp::as_millisecond);
            outcome.end = AgentEnd::Failed(err.message().to_owned());
        }
    }
    if let Some(id) = outcome.session_id.as_deref().and_then(|id| id.parse().ok())
        && let Ok(totals) = store.usage_totals(UsageScope::Lineage(id))
    {
        outcome.input_tokens =
            totals.tokens.input + totals.tokens.cache_read + totals.tokens.cache_write;
        outcome.output_tokens = totals.tokens.output;
        outcome.cost_usd = totals.cost_usd;
    }
    outcome.reached_model = matches!(outcome.end, AgentEnd::Completed(_))
        || outcome.input_tokens + outcome.output_tokens > 0;
    if !outcome.reached_model {
        outcome.end = unreached(outcome.end, closed);
    }
    outcome
}

/// A failure before any model response: a network or DNS error is
/// `Unreachable` (re-run automatically); every credential of the provider
/// benched until `closed` (Unix ms, no fallback answered) is a closed
/// usage window, `UsageClosed` (the job is held: no run can reach the
/// model before then); anything else stays a failure.
fn unreached(end: AgentEnd, closed: Option<i64>) -> AgentEnd {
    let AgentEnd::Failed(message) = end else {
        return end;
    };
    if let Some(until) = closed {
        return AgentEnd::UsageClosed(until);
    }
    if message.contains(": network error") {
        return AgentEnd::Unreachable(message);
    }
    AgentEnd::Failed(message)
}

async fn session(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    store: &Arc<Store>,
    request: &AgentRequest,
    outcome: &mut AgentOutcome,
) -> Result<AgentEnd, Error> {
    let job = &request.job;
    let channel = super::notepad::Channel::open(scope, store, job, &request.run_id)?;
    let ended = Box::pin(agent(scope, launch, store, request, &channel, outcome)).await;
    channel.settle(store);
    ended
}

async fn agent(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    store: &Arc<Store>,
    request: &AgentRequest,
    channel: &super::notepad::Channel,
    outcome: &mut AgentOutcome,
) -> Result<AgentEnd, Error> {
    let job = &request.job;
    let factory = factory(
        scope,
        launch,
        Arc::clone(store),
        job,
        request.replay,
        Some(channel.dir()),
    )?;
    let target = factory
        .models
        .primary(&flags(job, &scope.config().cron))
        .await?;
    let id = factory.create_session(None, &target)?;
    outcome.session_id = Some(id.to_string());
    let activity = Arc::new(Activity {
        started: Instant::now(),
        last: AtomicU64::new(0),
    });
    let clients = Clients {
        events: Arc::clone(&activity) as Arc<dyn EventSink>,
        secrets: Arc::new(NoPrompt),
        completions: None,
        sudo: None,
        delegation: None,
    };
    let live = factory.open_with(id, &target, Some(clients)).await?;
    let end = {
        let mut options = TurnOptions::new(&CancelToken::new());
        options.deltas = Arc::clone(&activity) as Arc<dyn DeltaSink>;
        let interrupt = options.interrupt.clone();
        let turn = live.agent.run_turn(
            Some(&live.scope),
            id,
            TurnInput::text(&request.prompt),
            options,
        );
        tokio::pin!(turn);
        let watched = watch(&activity, &interrupt, request.idle, request.wall);
        tokio::select! {
            result = &mut turn => turn_end(result),
            reason = watched => {
                // The turn was told to stop: let it wind down, briefly.
                let _ = tokio::time::timeout(STOP_GRACE, &mut turn).await;
                Ok(AgentEnd::TimedOut(reason))
            }
        }
    };
    let reason = if matches!(end, Ok(AgentEnd::Completed(_))) {
        EndReason::UserExit
    } else {
        EndReason::Error
    };
    live.close(store, reason).await;
    end
}

/// How an ended turn maps to a run's end; a benched-credentials refusal
/// stays an error so [`run`] can tell a closed usage window.
fn turn_end(result: Result<pmt_agent::turn::TurnResult, Error>) -> Result<AgentEnd, Error> {
    match result {
        Ok(result) if result.exit_reason == ExitReason::Completed => Ok(AgentEnd::Completed(
            result.final_response.unwrap_or_default(),
        )),
        Ok(result) => match result.error {
            Some(err) if pmt_providers::resolve::bench_end(&err).is_some() => Err(err),
            Some(err) => Ok(AgentEnd::Failed(format!(
                "{}: {}",
                result.exit_reason.as_str(),
                err.message()
            ))),
            None => Ok(AgentEnd::Failed(result.exit_reason.as_str().to_owned())),
        },
        Err(err) => Err(err),
    }
}

/// Resolves with `idle` or `wall` once a limit is passed, after asking the
/// turn to stop.
async fn watch(
    activity: &Activity,
    interrupt: &pmt_agent::turn::Interrupt,
    idle: Option<Duration>,
    wall: Option<Duration>,
) -> &'static str {
    loop {
        tokio::time::sleep(WATCH).await;
        let reason = if wall.is_some_and(|w| activity.started.elapsed() >= w) {
            "wall"
        } else if idle.is_some_and(|i| activity.idle() >= i) {
            "idle"
        } else {
            continue;
        };
        interrupt.stop(Issuer::System(format!("cron_{reason}_timeout")));
        return reason;
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentEnd, unreached};

    #[test]
    fn failures_before_the_model_are_classified() {
        assert_eq!(
            unreached(
                AgentEnd::Failed("provider_error: openai: network error: dns error".into()),
                None
            ),
            AgentEnd::Unreachable("provider_error: openai: network error: dns error".into())
        );
        assert_eq!(
            unreached(
                AgentEnd::Failed("every credential of 'x' is benched until ...".into()),
                Some(1_793_797_200_000)
            ),
            AgentEnd::UsageClosed(1_793_797_200_000)
        );
        assert_eq!(
            unreached(AgentEnd::Failed("anthropic: rate_limit error".into()), None),
            AgentEnd::Failed("anthropic: rate_limit error".into())
        );
    }
}
