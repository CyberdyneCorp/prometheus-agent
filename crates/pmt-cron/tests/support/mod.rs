//! A temporary profile with its store, a catalog that knows a few names
//! and a scripted runner.

#![allow(dead_code, unreachable_pub)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_cron::deliver::{SendError, Sender, Sent};
use pmt_cron::exec::{AgentEnd, AgentOutcome, AgentRequest, Runner, ScriptOutcome};
use pmt_cron::job::{Job, JobSpec};
use pmt_cron::ops::{self, Profile};
use pmt_cron::schedule::Schedule;
use pmt_cron::validate::Catalog;
use pmt_store::Store;
use pmt_store::cron::DeliveryRow;

pub struct TestProfile {
    _root: tempfile::TempDir,
    pub home: PathBuf,
    pub scope: ProfileScope,
}

impl TestProfile {
    pub fn new() -> Self {
        Self::with(&[])
    }

    pub fn with(overrides: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home_dir = root.path().join("home");
        let home = Home::open(&home_dir).unwrap();
        let dir = home
            .profile(&ProfileName::parse("default").unwrap())
            .unwrap();
        let scope =
            ProfileScope::open(dir, &Overrides::parse(overrides).unwrap(), Launch::Other).unwrap();
        std::fs::create_dir_all(scope.path("scripts")).unwrap();
        Self {
            _root: root,
            home: home_dir,
            scope,
        }
    }

    /// Opens the store (a second open is what a restarted process sees).
    pub fn store(&self) -> Store {
        Store::open(&self.scope).unwrap()
    }

    pub fn scripts(&self) -> PathBuf {
        self.scope.path("scripts")
    }
}

pub struct Known;

impl Catalog for Known {
    fn skill_exists(&self, name: &str) -> bool {
        name == "digest"
    }
    fn toolset_exists(&self, name: &str) -> bool {
        ["web", "file", "core"].contains(&name)
    }
    fn check_model(&self, _: Option<&str>, _: Option<&str>) -> Result<(), String> {
        Ok(())
    }
    fn profile_exists(&self, name: &str) -> bool {
        ["default", "assistant"].contains(&name)
    }
    fn secret_reserved(&self, name: &str) -> bool {
        name.ends_with("_API_KEY")
    }
}

pub fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

/// Creates a job at `now` in UTC.
pub fn create(store: &Store, scripts: &Path, spec: JobSpec, name: &str, now: Timestamp) -> Job {
    let profile = Profile {
        store,
        scripts_dir: scripts,
        zone: TimeZone::UTC,
        catalog: &Known,
    };
    let new = ops::NewJob {
        name: Some(name.to_owned()),
        ..ops::NewJob::default()
    };
    ops::create(&profile, spec, new, now).unwrap()
}

pub fn every(secs: u64, prompt: &str) -> JobSpec {
    JobSpec::new(Schedule::Interval { every_secs: secs }, prompt)
}

/// A runner answering from a script of outcomes, recording each call.
#[derive(Default)]
pub struct Scripted {
    pub answer: Mutex<Option<AgentEnd>>,
    pub preflight: Mutex<Option<String>>,
    pub script: Mutex<ScriptOutcome>,
    pub calls: Mutex<Vec<String>>,
    pub prompts: Mutex<Vec<String>>,
    pub skill_text: String,
    /// Bodies `fetch` answers, in order (the last repeats).
    pub pages: Mutex<Vec<String>>,
    /// Outcomes `run_agent` answers, in order, before `answer`.
    pub queue: Mutex<Vec<AgentEnd>>,
}

impl Scripted {
    pub fn answering(text: &str) -> Arc<Self> {
        let runner = Self::default();
        *runner.answer.lock().unwrap() = Some(AgentEnd::Completed(text.to_owned()));
        Arc::new(runner)
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Runner for Scripted {
    fn preflight<'a>(&'a self, _: &'a Job) -> BoxFuture<'a, Result<(), String>> {
        let missing = self.preflight.lock().unwrap().clone();
        Box::pin(async move { missing.map_or(Ok(()), Err) })
    }

    fn skill_text(&self, _: &Job) -> String {
        self.skill_text.clone()
    }

    fn run_script<'a>(
        &'a self,
        _: &'a Job,
        script: &'a Path,
        _: Duration,
    ) -> BoxFuture<'a, ScriptOutcome> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("script {}", script.display()));
        let outcome = self.script.lock().unwrap().clone();
        Box::pin(async move { outcome })
    }

    fn fetch<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<String, String>> {
        self.calls.lock().unwrap().push(format!("fetch {url}"));
        let mut pages = self.pages.lock().unwrap();
        let page = if pages.len() > 1 {
            Some(pages.remove(0))
        } else {
            pages.first().cloned()
        };
        Box::pin(async move { page.ok_or_else(|| "no page".to_owned()) })
    }

    fn run_agent(&self, request: AgentRequest) -> BoxFuture<'_, AgentOutcome> {
        self.calls.lock().unwrap().push("agent".to_owned());
        self.prompts.lock().unwrap().push(request.prompt);
        let queued = {
            let mut queue = self.queue.lock().unwrap();
            (!queue.is_empty()).then(|| queue.remove(0))
        };
        let end = queued
            .or_else(|| self.answer.lock().unwrap().clone())
            .unwrap_or(AgentEnd::Completed("done".into()));
        let reached_model = !matches!(end, AgentEnd::Unreachable(_) | AgentEnd::UsageClosed(_));
        Box::pin(async move {
            AgentOutcome {
                session_id: Some("ses_test".into()),
                end,
                input_tokens: 10,
                output_tokens: 5,
                cost_usd: 0.0,
                reached_model,
            }
        })
    }

    fn redact(&self, text: &str) -> String {
        text.replace("sk-secret", "[REDACTED]")
    }
}

/// A sender recording what it sent, failing per target.
pub struct Outbox {
    pub sent: Mutex<Vec<(String, String)>>,
    pub fail: Mutex<Option<SendError>>,
    /// Answer with a message id (`false`: acknowledged without one).
    pub with_ids: bool,
}

impl Default for Outbox {
    fn default() -> Self {
        Self {
            sent: Mutex::default(),
            fail: Mutex::default(),
            with_ids: true,
        }
    }
}

impl Sender for Outbox {
    fn send<'a>(&'a self, row: &'a DeliveryRow) -> BoxFuture<'a, Result<Sent, SendError>> {
        let fail = self.fail.lock().unwrap().clone();
        if fail.is_none() {
            self.sent
                .lock()
                .unwrap()
                .push((row.target.clone(), row.body.clone()));
        }
        let id = self.with_ids.then(|| format!("msg-{}", row.id));
        Box::pin(async move { fail.map_or(Ok(Sent { message_id: id }), Err) })
    }
}

/// The bodies sent so far.
impl Outbox {
    pub fn bodies(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| b.clone())
            .collect()
    }
}

/// The execution context of `profile` at `now` (UTC).
pub fn context<'a>(
    profile: &'a TestProfile,
    store: &'a Store,
    config: &'a pmt_config::schema::CronConfig,
    runner: &'a Scripted,
    now: Timestamp,
) -> pmt_cron::exec::Exec<'a> {
    let clock: &'static pmt_cron::clock::FakeClock =
        Box::leak(Box::new(pmt_cron::clock::FakeClock::new(now)));
    pmt_cron::exec::Exec {
        store,
        profile: "default",
        zone: Box::leak(Box::new(TimeZone::UTC)),
        scripts_dir: Box::leak(Box::new(profile.scripts())),
        config,
        runner,
        clock,
        text: &**Box::leak(Box::new(pmt_i18n::Strings::english(
            pmt_i18n::Surface::Core,
        ))),
    }
}

/// Ticks at `now`, executes every claimed run and sends the outbox;
/// returns the finished runs.
pub fn tick_all(
    profile: &TestProfile,
    store: &Store,
    runner: &Scripted,
    outbox: &Outbox,
    config: &pmt_config::schema::CronConfig,
    now: Timestamp,
) -> Vec<pmt_store::cron::RunRow> {
    let claimed =
        pmt_cron::scheduler::tick(store, &profile.home, now, &TimeZone::UTC, config).unwrap();
    let ctx = context(profile, store, config, runner, now);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut runs = Vec::new();
    for (job, run) in &claimed {
        runs.push(
            rt.block_on(pmt_cron::exec::execute(&ctx, job, run))
                .unwrap(),
        );
    }
    rt.block_on(pmt_cron::deliver::pump(store, outbox, now.as_millisecond()))
        .unwrap();
    runs
}

/// The profile operations' view of a test store.
pub fn ops_profile<'a>(store: &'a Store, scripts: &'a Path) -> Profile<'a> {
    Profile {
        store,
        scripts_dir: scripts,
        zone: TimeZone::UTC,
        catalog: &Known,
    }
}
