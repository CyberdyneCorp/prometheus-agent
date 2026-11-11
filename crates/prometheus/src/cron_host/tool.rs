//! The `cronjob` tool of interactive sessions (spec: scheduled-jobs,
//! Model tool surface; Canonical schedule echo and confirmation; Complete
//! cronjob tool field set; Asynchronous tool-initiated runs with per-run
//! context): create and schedule edits answer `needs_confirmation` with
//! the echo and a token valid for 10 minutes, and save only when the call
//! is repeated with it; a job created from a messaging chat records the
//! chat as its origin, so `deliver = origin` goes back there. The tool
//! never takes a model: `pinned` only freezes or releases the profile's
//! current main model.

use std::path::Path;
use std::sync::Arc;

use jiff::Timestamp;
use pmt_agent::tools::CronPort;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_cron::fields::{Fields, Pin};
use pmt_cron::job::Job;
use pmt_cron::ops;
use pmt_cron::schedule::Echo;
use pmt_cron::target::JobOrigin;
use pmt_store::cron::RunRow;
use pmt_store::{Origin, Store};
use pmt_tools::ToolResult;
use serde_json::{Value, json};

use super::caller::Caller;
use super::{HostCatalog, HostRunner, profile_zone, scripts_dir};

/// The outcome excerpt a finished tool run reports at most.
const EXCERPT_CHARS: usize = 2000;

/// The port of a session of `scope` whose reach is `caller`; `detached`
/// sessions (someone reads the transcript later) get tool runs in the
/// background, the others (`chat -q`, cron runs, workers) run them
/// synchronously.
#[must_use]
pub fn port(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    store: &Arc<Store>,
    detached: bool,
    caller: Caller,
) -> Arc<dyn CronPort> {
    Arc::new(ToolPort {
        caller,
        scope: scope.clone(),
        launch: launch.clone(),
        catalog: HostCatalog::new(scope, launch),
        store: Arc::clone(store),
        detached,
    })
}

struct ToolPort {
    caller: Caller,
    scope: ProfileScope,
    launch: LaunchEnv,
    catalog: HostCatalog,
    store: Arc<Store>,
    detached: bool,
}

impl CronPort for ToolPort {
    fn call(&self, session: SessionId, arguments: Value) -> BoxFuture<'_, ToolResult> {
        Box::pin(async move {
            let reply = if arguments["action"] == "run" {
                self.run_now(session, &arguments).await
            } else {
                self.run(session, &arguments)
            };
            match reply {
                Ok(reply) => ToolResult::ok(reply.to_string()),
                Err(err) => ToolResult::error(err.message()),
            }
        })
    }
}

fn text(arguments: &Value, key: &str) -> Option<String> {
    arguments[key].as_str().map(str::to_owned)
}

fn list(arguments: &Value, key: &str) -> Option<Vec<String>> {
    match &arguments[key] {
        Value::String(one) => Some(vec![one.clone()]),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
        ),
        _ => None,
    }
}

/// The tool arguments as job fields.
fn fields(arguments: &Value, scope: &ProfileScope) -> Result<Fields, Error> {
    let pin = match arguments["pinned"].as_bool() {
        Some(true) => Some(crate::cron_cmd::main_model(scope)?),
        Some(false) => Some(Pin::Release),
        None => None,
    };
    Ok(Fields {
        schedule: text(arguments, "schedule"),
        schedule_text: text(arguments, "schedule_text"),
        prompt: text(arguments, "prompt"),
        skills: list(arguments, "skills"),
        script: text(arguments, "script"),
        no_agent: arguments["no_agent"].as_bool(),
        deliver: list(arguments, "deliver"),
        failure_deliver: list(arguments, "failure_deliver"),
        attach_to_session: arguments["attach_to_session"].as_bool(),
        toolsets: list(arguments, "toolsets"),
        memory: text(arguments, "memory"),
        grace: text(arguments, "grace"),
        idle: arguments["timeout"]["idle"].as_str().map(str::to_owned),
        wall: arguments["timeout"]["wall"].as_str().map(str::to_owned),
        repeat: arguments["repeat"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok()),
        pin,
        reasoning_effort: None,
        timezone: text(arguments, "timezone"),
        workdir: text(arguments, "workdir"),
        script_secrets: list(arguments, "script_secrets"),
        interpreter: text(arguments, "interpreter"),
        monitor_script: text(arguments, "monitor_script"),
        monitor_url: text(arguments, "monitor_url"),
        context_from: list(arguments, "context_from"),
        continuity: arguments["continuity"].as_bool(),
        // The chain is the owner's routing decision: `cron create/edit
        // --fallback`, not the agent's.
        fallback: None,
    })
}

fn usage(message: impl Into<String>) -> Error {
    Error::new(&codes::CLI_USAGE, message)
}

/// The job's summary in tool replies and `cron list --json`: its fields
/// plus state, next run, last status, failure streak and pin.
#[must_use]
pub fn summary(job: &Job) -> Value {
    let mut fields = serde_json::to_value(&job.spec).unwrap_or_else(|_| json!({}));
    if let Some(map) = fields.as_object_mut() {
        map.insert("id".into(), json!(job.id()));
        map.insert("name".into(), json!(job.row.name));
        map.insert(
            "schedule".into(),
            json!(format!(
                "{} {}",
                job.spec.schedule.kind(),
                job.spec.schedule.canonical()
            )),
        );
        map.insert("state".into(), json!(job.row.state));
        map.insert(
            "next_run_at".into(),
            json!(job.next_run().map(|t| t.to_string())),
        );
        map.insert("last_status".into(), json!(job.row.last_status));
        map.insert("failure_streak".into(), json!(job.row.failure_streak));
        map.insert("pinned".into(), json!(job.spec.pinned()));
        map.insert("paused_reason".into(), json!(job.row.paused_reason));
        map.insert(
            "delivery_unverified".into(),
            json!(job.row.delivery_unverified),
        );
    }
    fields
}

fn needs_confirmation(echo: &Echo, token: &str) -> Value {
    json!({
        "status": "needs_confirmation",
        "echo": echo,
        "summary": echo.render(),
        "confirm_token": token,
        "expires_in_secs": ops::CONFIRM_TTL_MS / 1000,
        "next_step": "Show the schedule to the user; repeat this exact call with confirm_token only after they agree.",
    })
}

/// What a finished tool run reports to its session.
fn outcome(job: &Job, run: &RunRow) -> String {
    let excerpt: String = run
        .output
        .as_deref()
        .unwrap_or_default()
        .chars()
        .take(EXCERPT_CHARS)
        .collect();
    let next = job
        .next_run()
        .map_or_else(|| "none".to_owned(), |t| t.to_string());
    format!(
        "[cron run finished] job {} ({}) run {}: status {}{}; delivered to {}; next scheduled \
         run {next}\n\n{excerpt}",
        job.row.name,
        job.id(),
        run.id,
        run.status,
        run.error
            .as_deref()
            .map(|e| format!(" ({e})"))
            .unwrap_or_default(),
        run.delivered.as_deref().unwrap_or("[]"),
    )
}

impl ToolPort {
    fn run(&self, session: SessionId, arguments: &Value) -> Result<Value, Error> {
        let now = Timestamp::now();
        let action = arguments["action"].as_str().unwrap_or_default();
        let job = || {
            text(arguments, "job_id")
                .ok_or_else(|| usage(format!("cronjob {action} needs 'job_id'")))
        };
        match action {
            "create" => self.create(session, arguments, now),
            "list" => Ok(
                json!({"jobs": self.store.cron_jobs()?.into_iter().filter_map(|r| Job::from_row(r).ok()).map(|j| summary(&j)).collect::<Vec<_>>()}),
            ),
            "update" => self.update(session, &job()?, arguments, now),
            "pause" => Ok(summary(&ops::pause(
                &self.store,
                &job()?,
                text(arguments, "paused_reason").as_deref(),
                now,
            )?)),
            "resume" => Ok(summary(&ops::resume(
                &self.store,
                &job()?,
                now,
                &profile_zone(&self.scope)?,
            )?)),
            "remove" => Ok(json!({"removed": summary(&ops::remove(&self.store, &job()?)?)})),
            "runs" => self.runs(&job()?, arguments),
            other => Err(usage(format!(
                "unknown cronjob action '{other}' (create, list, update, pause, resume, remove, run, runs)"
            ))),
        }
    }

    fn profile<'a>(&'a self, scripts: &'a std::path::Path) -> Result<ops::Profile<'a>, Error> {
        Ok(ops::Profile {
            store: &self.store,
            scripts_dir: scripts,
            zone: profile_zone(&self.scope)?,
            catalog: &self.catalog,
        })
    }

    /// A workdir the model sets must lie inside the calling session's
    /// workspace: the run's sandbox may write there, and the confirm
    /// token is the model's own to repeat, so it is no human approval. A
    /// wider directory is a person's call (`prometheus cron add/edit
    /// --workdir`). An empty one (clears it) and one that is not an
    /// existing absolute directory are left to the shared validation.
    /// The resolved path is what gets saved: a link inside the workspace
    /// could be repointed after the check (runs refuse a workdir that no
    /// longer resolves to itself).
    fn check_workdir(&self, session: SessionId, fields: &mut Fields) -> Result<(), Error> {
        let Some(target) = fields
            .workdir
            .as_deref()
            .map(Path::new)
            .filter(|dir| dir.is_absolute() && dir.is_dir())
            .map(pmt_core::capability::canonical)
        else {
            return Ok(());
        };
        let workspace = self
            .store
            .session(session)?
            .cwd
            .map(|cwd| pmt_core::capability::canonical(Path::new(&cwd)));
        if workspace.is_some_and(|w| target.starts_with(w)) {
            fields.workdir = Some(target.to_string_lossy().into_owned());
            return Ok(());
        }
        Err(Error::new(
            &codes::CRON_INVALID_WORKDIR,
            format!(
                "workdir {} is outside this session's workspace; a person can set it with \
                 `prometheus cron add|edit --workdir`",
                target.display()
            ),
        ))
    }

    /// The call's job fields, refused when they reach past the session.
    fn model_fields(&self, session: SessionId, arguments: &Value) -> Result<Fields, Error> {
        let mut fields = fields(arguments, &self.scope)?;
        self.check_workdir(session, &mut fields)?;
        self.caller.check(&fields)?;
        Ok(fields)
    }

    fn origin(&self, session: SessionId) -> Option<JobOrigin> {
        match self.store.session(session).ok()?.origin {
            Origin::Gateway {
                platform,
                chat,
                thread,
                ..
            } => Some(JobOrigin::Chat {
                platform,
                chat,
                thread,
            }),
            _ => None,
        }
    }

    fn create(
        &self,
        session: SessionId,
        arguments: &Value,
        now: Timestamp,
    ) -> Result<Value, Error> {
        let scripts = scripts_dir(&self.scope);
        let profile = self.profile(&scripts)?;
        let fields = self.model_fields(session, arguments)?;
        let mut spec = fields.new_spec(now, &profile.zone)?;
        let origin = self.origin(session);
        if spec.deliver.is_empty() {
            spec.deliver = ops::default_deliver(origin.as_ref());
        }
        let name = text(arguments, "name");
        let echo = ops::check(&profile, &spec, now)?;
        match text(arguments, "confirm_token") {
            Some(token) if ops::redeem(&token, &spec, name.as_deref(), now) => {
                let paused = arguments["paused"]
                    .as_bool()
                    .unwrap_or(false)
                    .then(|| text(arguments, "paused_reason").unwrap_or_default());
                let new = ops::NewJob {
                    name,
                    origin: origin.as_ref(),
                    paused,
                };
                let job = ops::create(&profile, spec, new, now)?;
                Ok(json!({"status": "created", "job": summary(&job), "summary": echo.render()}))
            }
            Some(_) => Err(Error::new(
                &codes::CRON_CONFIRMATION,
                "the confirm_token is unknown, expired or for different job fields: ask again without it",
            )),
            None => Ok(needs_confirmation(
                &echo,
                &ops::confirm_token(&spec, name.as_deref(), now),
            )),
        }
    }

    fn update(
        &self,
        session: SessionId,
        key: &str,
        arguments: &Value,
        now: Timestamp,
    ) -> Result<Value, Error> {
        let scripts = scripts_dir(&self.scope);
        let profile = self.profile(&scripts)?;
        let fields = self.model_fields(session, arguments)?;
        let name = text(arguments, "name");
        if fields.reschedules() {
            let current = ops::find(&self.store, key)?;
            let mut spec = current.spec.clone();
            fields.apply(&mut spec, now, &profile.zone)?;
            let echo = ops::check(&profile, &spec, now)?;
            let confirmed = text(arguments, "confirm_token")
                .is_some_and(|token| ops::redeem(&token, &spec, name.as_deref(), now));
            if !confirmed {
                return Ok(needs_confirmation(
                    &echo,
                    &ops::confirm_token(&spec, name.as_deref(), now),
                ));
            }
        }
        let (mut job, _) = ops::update(
            &profile,
            key,
            name.as_deref(),
            |spec| fields.apply(spec, now, &profile.zone),
            now,
        )?;
        match arguments["paused"].as_bool() {
            Some(true) => {
                let reason = text(arguments, "paused_reason");
                job = ops::pause(&self.store, job.id(), reason.as_deref(), now)?;
            }
            Some(false) => job = ops::resume(&self.store, job.id(), now, &profile.zone)?,
            None => {}
        }
        Ok(json!({"status": "updated", "job": summary(&job)}))
    }

    /// `run`: queued with the call's `prompt` as this run's context only;
    /// started in the background (the outcome re-enters the session as a
    /// notice) or, for sessions that cannot receive it, run to its end.
    async fn run_now(&self, session: SessionId, arguments: &Value) -> Result<Value, Error> {
        let key = text(arguments, "job_id").ok_or_else(|| usage("cronjob run needs 'job_id'"))?;
        let how = ops::QueueRun {
            context: text(arguments, "prompt").filter(|p| !p.trim().is_empty()),
            exclusive: true,
            ..ops::QueueRun::default()
        };
        let (job, run) = ops::queue_run(&self.store, &key, Timestamp::now(), &how)?;
        let runner = HostRunner::new(&self.scope, &self.launch, Arc::clone(&self.store));
        if !self.detached {
            let finished = runner.execute(&job, &run).await?;
            return Ok(json!({"status": "finished", "run_id": finished.id,
                "outcome": outcome(&job, &finished)}));
        }
        let store = Arc::clone(&self.store);
        let run_id = run.id.clone();
        pmt_core::spawn::spawn(async move {
            let text = match runner.execute(&job, &run).await {
                Ok(finished) => {
                    let job = ops::find(&store, job.id()).unwrap_or(job);
                    outcome(&job, &finished)
                }
                Err(err) => format!("[cron run failed to record] run {}: {err}", run.id),
            };
            let row = pmt_agent::turn::transcript::notice_row(&text, "cron");
            if let Err(err) = store.append_message(session, &row) {
                tracing::warn!(error = %err, "cron run outcome not reported to its session");
            }
        });
        Ok(json!({"status": "started", "run_id": run_id}))
    }

    fn runs(&self, key: &str, arguments: &Value) -> Result<Value, Error> {
        let job = ops::find(&self.store, key)?;
        let limit = arguments["limit"].as_u64().unwrap_or(10).clamp(1, 100);
        let runs = self.store.cron_runs(
            job.id(),
            text(arguments, "status").as_deref(),
            usize::try_from(limit).unwrap_or(10),
        )?;
        let runs: Vec<Value> = runs
            .iter()
            .map(|r| json!({"run_id": r.id, "scheduled_at": pmt_cron::schedule::at_ms(r.scheduled_at).to_string(),
                "status": r.status, "error": r.error, "session_id": r.session_id,
                "output": r.output.as_deref().map(|o| o.chars().take(500).collect::<String>())}))
            .collect();
        Ok(json!({"job": job.id(), "runs": runs}))
    }
}
