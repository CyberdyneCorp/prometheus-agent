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
    scope.patched(move |config| {
        config.memory.nudge_interval = 0;
        if memory == MemoryMode::Off {
            config.memory.memory_enabled = false;
            config.memory.user_profile_enabled = false;
        }
    })
}

fn flags(job: &Job) -> RouteFlags<'_> {
    RouteFlags {
        provider: job.spec.provider.as_deref(),
        model: job.spec.model.as_deref(),
        mock_script: None,
    }
}

fn factory(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    store: Arc<Store>,
    job: &Job,
    replay: bool,
) -> Result<Factory, Error> {
    let config = &scope.config().cron;
    let workspace = super::workspace(scope);
    std::fs::create_dir_all(&workspace).map_err(|e| {
        Error::new(
            &pmt_core::error::codes::OUTPUT_IO,
            format!("cannot create {}: {e}", workspace.display()),
        )
    })?;
    let options = Options {
        interactive: false,
        cwd: workspace,
        skills: job.spec.skills.clone(),
        no_context_files: true,
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
        }),
        ..Options::default()
    };
    Factory::with_store(
        run_scope(scope, job),
        store,
        launch,
        &flags(job),
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
    let factory = factory(scope, launch, store, job, false).map_err(|e| e.message().to_owned())?;
    factory
        .models
        .primary(&flags(job))
        .await
        .map(drop)
        .map_err(|e| e.message().to_owned())
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
    };
    match Box::pin(session(scope, launch, &store, &request, &mut outcome)).await {
        Ok(end) => outcome.end = end,
        Err(err) => outcome.end = AgentEnd::Failed(err.message().to_owned()),
    }
    if let Some(id) = outcome.session_id.as_deref().and_then(|id| id.parse().ok())
        && let Ok(totals) = store.usage_totals(UsageScope::Lineage(id))
    {
        outcome.input_tokens =
            totals.tokens.input + totals.tokens.cache_read + totals.tokens.cache_write;
        outcome.output_tokens = totals.tokens.output;
        outcome.cost_usd = totals.cost_usd;
    }
    outcome
}

async fn session(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    store: &Arc<Store>,
    request: &AgentRequest,
    outcome: &mut AgentOutcome,
) -> Result<AgentEnd, Error> {
    let job = &request.job;
    let factory = factory(scope, launch, Arc::clone(store), job, request.replay)?;
    let target = factory.models.primary(&flags(job)).await?;
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
                AgentEnd::TimedOut(reason)
            }
        }
    };
    let reason = if matches!(end, AgentEnd::Completed(_)) {
        EndReason::UserExit
    } else {
        EndReason::Error
    };
    live.close(store, reason).await;
    Ok(end)
}

/// How an ended turn maps to a run's end.
fn turn_end(result: Result<pmt_agent::turn::TurnResult, Error>) -> AgentEnd {
    match result {
        Ok(result) if result.exit_reason == ExitReason::Completed => {
            AgentEnd::Completed(result.final_response.unwrap_or_default())
        }
        Ok(result) => AgentEnd::Failed(match result.error {
            Some(err) => format!("{}: {}", result.exit_reason.as_str(), err.message()),
            None => result.exit_reason.as_str().to_owned(),
        }),
        Err(err) => AgentEnd::Failed(err.message().to_owned()),
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
