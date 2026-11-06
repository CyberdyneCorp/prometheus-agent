//! The `cronjob` tool of interactive sessions (spec: scheduled-jobs,
//! Model tool surface; Canonical schedule echo and confirmation): create
//! and schedule edits answer `needs_confirmation` with the echo and a
//! token valid for 10 minutes, and save only when the call is repeated
//! with it; a job created from a messaging chat records the chat as its
//! origin, so `deliver = origin` goes back there.

use std::sync::Arc;

use jiff::Timestamp;
use pmt_agent::tools::CronPort;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_cron::fields::Fields;
use pmt_cron::job::Job;
use pmt_cron::ops;
use pmt_cron::schedule::Echo;
use pmt_cron::target::JobOrigin;
use pmt_store::{Origin, Store};
use pmt_tools::ToolResult;
use serde_json::{Value, json};

use super::{HostCatalog, profile_zone, scripts_dir};

/// The port of a session of `scope`.
#[must_use]
pub fn port(scope: &ProfileScope, launch: &LaunchEnv, store: &Arc<Store>) -> Arc<dyn CronPort> {
    Arc::new(ToolPort {
        scope: scope.clone(),
        catalog: HostCatalog::new(scope, launch),
        store: Arc::clone(store),
    })
}

struct ToolPort {
    scope: ProfileScope,
    catalog: HostCatalog,
    store: Arc<Store>,
}

impl CronPort for ToolPort {
    fn call(&self, session: SessionId, arguments: Value) -> BoxFuture<'_, ToolResult> {
        Box::pin(async move {
            match self.run(session, &arguments) {
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
    arguments[key].as_array().map(|items| {
        items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    })
}

/// The tool arguments as job fields.
fn fields(arguments: &Value) -> Fields {
    Fields {
        schedule: text(arguments, "schedule"),
        schedule_text: text(arguments, "schedule_text"),
        prompt: text(arguments, "prompt"),
        skills: list(arguments, "skills"),
        script: text(arguments, "script"),
        no_agent: arguments["no_agent"].as_bool(),
        deliver: list(arguments, "deliver"),
        toolsets: list(arguments, "toolsets"),
        memory: text(arguments, "memory"),
        grace: text(arguments, "grace"),
        idle: arguments["timeout"]["idle"].as_str().map(str::to_owned),
        wall: arguments["timeout"]["wall"].as_str().map(str::to_owned),
        repeat: arguments["repeat"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok()),
        model: text(arguments, "model"),
        provider: text(arguments, "provider"),
        timezone: None,
    }
}

fn usage(message: impl Into<String>) -> Error {
    Error::new(&codes::CLI_USAGE, message)
}

/// The job's summary in tool replies.
#[must_use]
pub fn summary(job: &Job) -> Value {
    json!({
        "id": job.id(),
        "name": job.row.name,
        "schedule": format!("{} {}", job.spec.schedule.kind(), job.spec.schedule.canonical()),
        "state": job.row.state,
        "next_run_at": job.next_run().map(|t| t.to_string()),
        "last_status": job.row.last_status,
        "deliver": job.spec.deliver,
    })
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

impl ToolPort {
    fn run(&self, session: SessionId, arguments: &Value) -> Result<Value, Error> {
        let now = Timestamp::now();
        let action = arguments["action"].as_str().unwrap_or_default();
        let job =
            || text(arguments, "job").ok_or_else(|| usage(format!("cronjob {action} needs 'job'")));
        match action {
            "create" => self.create(session, arguments, now),
            "list" => Ok(
                json!({"jobs": self.store.cron_jobs()?.into_iter().filter_map(|r| Job::from_row(r).ok()).map(|j| summary(&j)).collect::<Vec<_>>()}),
            ),
            "update" => self.update(&job()?, arguments, now),
            "pause" => Ok(summary(&ops::pause(
                &self.store,
                &job()?,
                text(arguments, "reason").as_deref(),
                now,
            )?)),
            "resume" => Ok(summary(&ops::resume(
                &self.store,
                &job()?,
                now,
                &profile_zone(&self.scope)?,
            )?)),
            "remove" => Ok(json!({"removed": summary(&ops::remove(&self.store, &job()?)?)})),
            "run" => {
                let (job, run) = ops::manual_run(&self.store, &job()?, now)?;
                Ok(
                    json!({"status": "queued", "job": job.id(), "run_id": run.id,
                    "note": "the gateway daemon runs it at its next tick (or `prometheus cron tick`)"}),
                )
            }
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
        let mut spec = fields(arguments).new_spec(now, &profile.zone)?;
        let origin = self.origin(session);
        if spec.deliver.is_empty() {
            spec.deliver = ops::default_deliver(origin.as_ref());
        }
        let name = text(arguments, "name");
        let echo = ops::check(&profile, &spec, now)?;
        match text(arguments, "confirm_token") {
            Some(token) if ops::redeem(&token, &spec, name.as_deref(), now) => {
                let job = ops::create(&profile, spec, name, origin.as_ref(), now)?;
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

    fn update(&self, key: &str, arguments: &Value, now: Timestamp) -> Result<Value, Error> {
        let scripts = scripts_dir(&self.scope);
        let profile = self.profile(&scripts)?;
        let fields = fields(arguments);
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
        let (job, _) = ops::update(
            &profile,
            key,
            name.as_deref(),
            |spec| fields.apply(spec, now, &profile.zone),
            now,
        )?;
        Ok(json!({"status": "updated", "job": summary(&job)}))
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
