//! A profile with evolution on, a queued job, and test doubles of the
//! engine's ports: rollouts and improver sessions whose behaviour each
//! test scripts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::schema::EvolveMode;
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::Error;
use pmt_core::secret::Redactor;
use pmt_evolve::engine::gate::Limits;
use pmt_evolve::engine::ports::{
    BoxFuture, Entry, EntryKind, Improver, ImproverFailure, ImproverReply, ImproverRequest,
    RolloutOutcome, RolloutRequest, Rollouts, SessionKind,
};
use pmt_evolve::engine::{Engine, Finished};
use pmt_evolve::harness::Rules;
use pmt_evolve::job::{self, Profile, Request, TaskInput, Trigger};

pub(crate) struct Fixture {
    pub(crate) root: tempfile::TempDir,
    pub(crate) scope: ProfileScope,
}

impl Fixture {
    pub(crate) fn new(overrides: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(&root.path().join("home")).unwrap();
        let profile = home.profile(&ProfileName::parse("work").unwrap()).unwrap();
        let mut all = vec!["evolve.enabled=true"];
        all.extend_from_slice(overrides);
        let scope =
            ProfileScope::open(profile, &Overrides::parse(&all).unwrap(), Launch::Other).unwrap();
        Self { root, scope }
    }

    pub(crate) fn dir(&self) -> &Path {
        self.scope.dir()
    }

    /// Queues a job on a task file whose workspace holds `ledger.csv`.
    pub(crate) fn job(&self, k: u8, mode: EvolveMode) -> String {
        self.job_with(k, mode, None, &[])
    }

    pub(crate) fn job_with(
        &self,
        k: u8,
        mode: EvolveMode,
        budget: Option<f64>,
        files: &[(&str, &str)],
    ) -> String {
        self.job_in(k, mode, budget, files, Some("accounting"))
    }

    /// Queues a job whose task file names `domain` (none: the classifier
    /// resolves it).
    pub(crate) fn job_in(
        &self,
        k: u8,
        mode: EvolveMode,
        budget: Option<f64>,
        files: &[(&str, &str)],
        domain: Option<&str>,
    ) -> String {
        let workspace = self
            .root
            .path()
            .join(format!("ws-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("ledger.csv"), "amount\n10.00\n-2.50\n").unwrap();
        for (path, text) in files {
            let file = workspace.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        let task = workspace.with_extension("toml");
        std::fs::write(
            &task,
            format!(
                "statement = \"Sum the amounts in ledger.csv\"\n{}workspace = \"{}\"\n",
                domain
                    .map(|d| format!("domain = \"{d}\"\n"))
                    .unwrap_or_default(),
                workspace.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
        let profile = Profile {
            dir: self.dir(),
            name: "work",
            config: self.scope.config(),
            store: None,
            redactor: Redactor::default(),
        };
        let request = Request {
            k: Some(k),
            mode: Some(mode),
            budget_usd: budget,
            ..Request::new(Trigger::Cli, TaskInput::File(task))
        };
        job::queue(&profile, &request).unwrap().id
    }

    pub(crate) fn job_dir(&self, id: &str) -> PathBuf {
        job::Jobs::of(self.dir()).dir(id)
    }

    pub(crate) fn rules() -> Rules {
        Rules::with_toolsets(["file".to_owned(), "terminal".to_owned()])
    }

    /// Runs `job` with the doubles and limits of 2 sessions, 2 rollouts.
    pub(crate) fn run(
        &self,
        job: &str,
        rollouts: &FakeRollouts,
        improver: &FakeImprover,
    ) -> Result<Finished, Error> {
        self.run_limited(job, rollouts, improver, Arc::new(Limits::new(2, 2)))
    }

    pub(crate) fn run_limited(
        &self,
        job: &str,
        rollouts: &FakeRollouts,
        improver: &FakeImprover,
        limits: Arc<Limits>,
    ) -> Result<Finished, Error> {
        let rules = Self::rules();
        let engine = Engine {
            profile_dir: self.dir(),
            config: &self.scope.config().evolve,
            rules: &rules,
            rollouts,
            improver,
            limits,
            retry_delay: Duration::ZERO,
            experience: &self.scope.config().experience,
            estimate_usd: 0.0,
        };
        runtime().block_on(engine.run(job))
    }
}

pub(crate) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

type RolloutFn = dyn Fn(&RolloutRequest<'_>) -> RolloutOutcome + Send + Sync;

/// Rollouts that answer with `behave` (default: ok, the final message
/// names the candidate's GUARDRAILS.md), after an optional per-label
/// delay, recording every label run.
pub(crate) struct FakeRollouts {
    pub(crate) calls: Mutex<Vec<String>>,
    pub(crate) finished: Mutex<Vec<String>>,
    pub(crate) behave: Box<RolloutFn>,
    pub(crate) delays: HashMap<String, u64>,
    pub(crate) usd: f64,
}

impl Default for FakeRollouts {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            finished: Mutex::new(Vec::new()),
            behave: Box::new(ok_rollout),
            delays: HashMap::new(),
            usd: 0.01,
        }
    }
}

impl FakeRollouts {
    pub(crate) fn with(
        behave: impl Fn(&RolloutRequest<'_>) -> RolloutOutcome + Send + Sync + 'static,
    ) -> Self {
        Self {
            behave: Box::new(behave),
            ..Self::default()
        }
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

pub(crate) fn ok_rollout(request: &RolloutRequest<'_>) -> RolloutOutcome {
    let guardrails =
        std::fs::read_to_string(request.harness.join("GUARDRAILS.md")).unwrap_or_default();
    let message = format!("Total: 7.50 [{}]", guardrails.lines().count());
    RolloutOutcome {
        ok: true,
        final_message: Some(message.clone()),
        entries: vec![
            Entry::new(EntryKind::User, "Sum the amounts in ledger.csv"),
            Entry::new(EntryKind::ToolCall, "{\"path\": \"ledger.csv\"}").tool("read_file"),
            Entry::new(EntryKind::ToolResult, "amount\n10.00\n-2.50\n").tool("read_file"),
            Entry::new(EntryKind::Assistant, message),
        ],
        ..RolloutOutcome::default()
    }
}

impl Rollouts for FakeRollouts {
    fn run<'a>(&'a self, request: RolloutRequest<'a>) -> BoxFuture<'a, RolloutOutcome> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(request.label.to_owned());
            if let Some(ms) = self.delays.get(request.label) {
                tokio::time::sleep(Duration::from_millis(*ms)).await;
            }
            let mut outcome = (self.behave)(&request);
            outcome.usd = self.usd;
            self.finished.lock().unwrap().push(request.label.to_owned());
            outcome
        })
    }

    fn model_id(&self) -> String {
        "mock/rollout".to_owned()
    }
}

/// What an improver double answers: the request and how many times this
/// label ran before.
type ImproverFn =
    dyn Fn(&ImproverRequest<'_>, usize) -> Result<ImproverReply, ImproverFailure> + Send + Sync;

/// Improver sessions that answer with `behave` (default: append a rule
/// line to GUARDRAILS.md; a selector picks the first candidate),
/// recording every (label, mission).
pub(crate) struct FakeImprover {
    pub(crate) calls: Mutex<Vec<(String, String)>>,
    pub(crate) behave: Box<ImproverFn>,
    pub(crate) usd: f64,
    pub(crate) active: Mutex<(usize, usize)>,
}

impl Default for FakeImprover {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            behave: Box::new(|r, _| Ok(edit_guardrails(r, &format!("- rule from {}", r.label)))),
            usd: 0.02,
            active: Mutex::new((0, 0)),
        }
    }
}

impl FakeImprover {
    pub(crate) fn with(
        behave: impl Fn(&ImproverRequest<'_>, usize) -> Result<ImproverReply, ImproverFailure>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            behave: Box::new(behave),
            ..Self::default()
        }
    }

    pub(crate) fn labels(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(l, _)| l.clone())
            .collect()
    }

    pub(crate) fn mission(&self, label: &str) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(l, _)| l == label)
            .map(|(_, m)| m.clone())
            .collect()
    }

    /// The most sessions that ran at once.
    pub(crate) fn peak(&self) -> usize {
        self.active.lock().unwrap().1
    }
}

/// The default answer: a selector picks the first candidate; any other
/// session appends `line` to GUARDRAILS.md.
pub(crate) fn edit_guardrails(request: &ImproverRequest<'_>, line: &str) -> ImproverReply {
    if request.kind == SessionKind::Select {
        let dir = request.workspace.join("context").join("candidates");
        let mut ids: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .trim_end_matches(".md")
                    .to_owned()
            })
            .collect();
        ids.sort();
        return ImproverReply {
            text: format!("```choice\n{}\n```\n<why>it states the total</why>", ids[0]),
            usd: 0.0,
        };
    }
    let path = request.workspace.join("context/harness/GUARDRAILS.md");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    text.push_str(line);
    text.push('\n');
    std::fs::write(&path, text).unwrap();
    ImproverReply {
        text: format!("Added a rule (rollouts/base1.md, step 4): {line}"),
        usd: 0.0,
    }
}

impl Improver for FakeImprover {
    fn run<'a>(
        &'a self,
        request: ImproverRequest<'a>,
    ) -> BoxFuture<'a, Result<ImproverReply, ImproverFailure>> {
        Box::pin(async move {
            let seen = {
                let mut calls = self.calls.lock().unwrap();
                let seen = calls.iter().filter(|(l, _)| l == request.label).count();
                calls.push((request.label.to_owned(), request.mission.to_owned()));
                seen
            };
            {
                let mut active = self.active.lock().unwrap();
                active.0 += 1;
                active.1 = active.1.max(active.0);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            let reply = (self.behave)(&request, seen);
            self.active.lock().unwrap().0 -= 1;
            reply.map(|mut r| {
                r.usd = self.usd;
                r
            })
        })
    }

    fn model_id(&self) -> String {
        "mock/improver".to_owned()
    }
}

/// A reflector reply: two edit rows and, with `evidence`, a lesson of
/// `confidence`; without, `NO_ENTRY`.
pub(crate) fn reflection_reply(confidence: &str, evidence: Option<&str>) -> String {
    let edits = "<edits>\ncomponent: GUARDRAILS.md\ncandidates: inv1, agg1\n\
                 change: a rule line\nobserved: base1 step 4 kept the refund\nverdict: helped\n\
                 </edits>\n";
    match evidence {
        Some(evidence) => format!(
            "{edits}<experience>\ncue: ledgers with refunds\nlesson: sum signed amounts\n\
             evidence: {evidence}\nconfidence: {confidence} because two rollouts agree\n</experience>"
        ),
        None => format!("{edits}NO_ENTRY"),
    }
}

/// An improver whose reflector answers with `reply` and whose other
/// sessions edit GUARDRAILS.md.
pub(crate) fn reflecting(reply: String) -> FakeImprover {
    FakeImprover::with(move |r, _| {
        if r.kind == SessionKind::Reflect {
            return Ok(ImproverReply {
                text: reply.clone(),
                usd: 0.0,
            });
        }
        Ok(edit_guardrails(r, &format!("- rule from {}", r.label)))
    })
}

pub(crate) fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}
