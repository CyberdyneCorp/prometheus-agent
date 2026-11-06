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
use pmt_cron::deliver::{SendError, Sender};
use pmt_cron::exec::{AgentEnd, AgentOutcome, AgentRequest, Runner, ScriptOutcome};
use pmt_cron::job::{Job, JobSpec};
use pmt_cron::ops::{self, Profile};
use pmt_cron::schedule::Schedule;
use pmt_cron::validate::Catalog;
use pmt_store::Store;

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
    ops::create(&profile, spec, Some(name.to_owned()), None, now).unwrap()
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

    fn run_agent(&self, request: AgentRequest) -> BoxFuture<'_, AgentOutcome> {
        self.calls.lock().unwrap().push("agent".to_owned());
        self.prompts.lock().unwrap().push(request.prompt);
        let end = self
            .answer
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(AgentEnd::Completed("done".into()));
        Box::pin(async move {
            AgentOutcome {
                session_id: Some("ses_test".into()),
                end,
                input_tokens: 10,
                output_tokens: 5,
                cost_usd: 0.0,
            }
        })
    }

    fn redact(&self, text: &str) -> String {
        text.replace("sk-secret", "[REDACTED]")
    }
}

/// A sender recording what it sent, failing per target.
#[derive(Default)]
pub struct Outbox {
    pub sent: Mutex<Vec<(String, String)>>,
    pub fail: Mutex<Option<SendError>>,
}

impl Sender for Outbox {
    fn send<'a>(
        &'a self,
        target: &'a str,
        body: &'a str,
        _: bool,
    ) -> BoxFuture<'a, Result<(), SendError>> {
        let fail = self.fail.lock().unwrap().clone();
        if fail.is_none() {
            self.sent
                .lock()
                .unwrap()
                .push((target.to_owned(), body.to_owned()));
        }
        Box::pin(async move { fail.map_or(Ok(()), Err) })
    }
}
