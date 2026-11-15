//! `prometheus demo --evolve` and `prometheus evolve demo` (spec:
//! replay-and-evaluation, Offline self-improvement demo; harness-evolution,
//! Offline demo pipeline): two ledger-totalling tasks of the `accounting`
//! domain go through the whole evolution pipeline — base rollouts,
//! proposals, probes, composition, blind selection, reflection and the
//! experience library — offline, in a profile of the demo's own, with the
//! deterministic mock provider playing every model and the base harness's
//! `scripts/total.py` adding only the positive amounts.
//!
//! The demo's rollouts copy the candidate harness's `scripts/` into the
//! rollout workspace, where the scripted agent reads the script and
//! reports the total it computes. After each job the demo adopts the
//! selected proposal (standing in for the user's approval, which harness
//! script changes always need), so the library gains the job's lesson.
//! Its state lives in the output directory: running the same command
//! again resumes the jobs instead of starting new ones, so a finished
//! demo reports zero new executions and editor calls.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::harness::{HarnessDir, Source as HarnessSource, Tree};
use pmt_config::schema::EvolveMode;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_evolve::engine::Engine;
use pmt_evolve::engine::gate::Limits;
use pmt_evolve::engine::ledger::{Kind, Ledger};
use pmt_evolve::engine::ports::{BoxFuture, RolloutOutcome, RolloutRequest, Rollouts};
use pmt_evolve::engine::summary::JobSummary;
use pmt_evolve::job::{Jobs, Request, TaskInput, Trigger};
use pmt_store::{Store, UsageScope};
use serde::{Deserialize, Serialize};

use super::Place;
use crate::context::Context;
use crate::evolve_host::{improver::HostImprover, rollout::HostRollouts};

const MOCK: &str = include_str!("evolve_scenario/mock.toml");
const BUGGY: &str = include_str!("evolve_scenario/total_buggy.py");
const FIXED: &str = include_str!("evolve_scenario/total_fixed.py");

/// The file that marks a directory as this demo's (and holds its state).
const STATE: &str = "demo-state.json";

/// The two tasks: id, ledger, the total the fixed script reports.
const TASKS: [(&str, &str, &str); 2] = [
    ("ledger-a", "100.00\n60.25\n-75.10\n", "Total: 85.15"),
    ("ledger-b", "30.00\n-9.75\n", "Total: 20.25"),
];

/// What the demo runs with.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Candidates per mode.
    pub k: u8,
    /// The modes.
    pub mode: EvolveMode,
    /// `--output`.
    pub output: Option<PathBuf>,
}

/// One task's job, kept across runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TaskState {
    job: String,
    adopted: Vec<String>,
}

/// `demo-state.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    tasks: std::collections::BTreeMap<String, TaskState>,
}

/// What one task came to.
#[derive(Debug, Clone, Serialize)]
pub struct TaskReport {
    /// The task.
    pub task: String,
    /// Its job.
    pub job: String,
    /// The selected candidates' final answers (one per mode).
    pub deliverables: Vec<String>,
    /// What was expected.
    pub expected: String,
    /// Every selected deliverable matched.
    pub completed: bool,
    /// The job had finished before this run.
    pub resumed: bool,
}

/// `demo.json`.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Tasks whose selected deliverables were right.
    pub tasks_completed: usize,
    /// The others.
    pub tasks_failed: usize,
    /// Rollouts executed by this run (cache hits excluded).
    pub executions: usize,
    /// Proposer sessions executed by this run (cache hits excluded).
    pub editor_calls: usize,
    /// Model calls made by this run.
    pub model_calls: u64,
    /// Per task.
    pub tasks: Vec<TaskReport>,
}

/// Rollouts that see the candidate's `scripts/` in their workspace.
struct DemoRollouts {
    inner: HostRollouts,
}

/// `task` with the candidate's scripts added to its source, beside it in
/// the job directory (the rollout's job-relative paths stay right).
fn with_scripts(task: &Path, harness: &Path, label: &str) -> std::io::Result<PathBuf> {
    let job = task.parent().unwrap_or(task);
    let dir = job.join(format!("demo-task-{label}-{}", ulid::Ulid::generate()));
    pmt_core::private_fs::copy_tree(task, &dir)?;
    let scripts = harness.join("scripts");
    if scripts.is_dir() {
        pmt_core::private_fs::copy_tree(&scripts, &dir.join("source").join("scripts"))?;
    }
    Ok(dir)
}

impl Rollouts for DemoRollouts {
    fn run<'a>(&'a self, request: RolloutRequest<'a>) -> BoxFuture<'a, RolloutOutcome> {
        Box::pin(async move {
            let task = match with_scripts(request.task, request.harness, request.label) {
                Ok(dir) => dir,
                Err(e) => return RolloutOutcome::failed(format!("demo task: {e}")),
            };
            let outcome = self
                .inner
                .run(RolloutRequest {
                    task: &task,
                    ..request
                })
                .await;
            let _ = std::fs::remove_dir_all(&task);
            outcome
        })
    }

    fn model_id(&self) -> String {
        self.inner.model_id()
    }
}

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::io(&codes::DEMO_IO, path, e)
}

/// The demo profile's overrides: offline, cached, evolution on, every
/// model the mock.
fn overrides(place: &Place) -> Vec<String> {
    let script = place.root().join("mock.toml");
    let mut set = vec![
        "lsp.enabled=false".to_owned(),
        "replay.cache.mode=read_write".to_owned(),
        "evolve.enabled=true".to_owned(),
        "evolve.adoption.policy=\"never\"".to_owned(),
        "model.provider=\"mock\"".to_owned(),
        "model.default=\"mock\"".to_owned(),
        format!(
            "providers.mock.script={}",
            toml::Value::String(script.to_string_lossy().into_owned())
        ),
    ];
    if !super::HAS_OS_SANDBOX {
        set.push(r#"security.sandbox.allow_degraded=["all"]"#.to_owned());
    }
    set
}

/// Writes the mock script, the task files and their ledgers.
fn write_scenario(place: &Place) -> Result<(), Error> {
    let script = place.root().join("mock.toml");
    std::fs::write(&script, MOCK.replace("@FIXED@", FIXED)).map_err(|e| io(&script, &e))?;
    for (id, _, _) in TASKS {
        let dir = place.root().join("tasks").join(id);
        std::fs::create_dir_all(&dir).map_err(|e| io(&dir, &e))?;
        let letter = id.trim_start_matches("ledger-").to_uppercase();
        let task = format!(
            "statement = \"LEDGER-{letter}: total the signed amounts of ledger.txt with the harness script scripts/total.py and report `Total: <amount>`.\"\ndomain = \"accounting\"\ntoolsets = [\"file\"]\n"
        );
        std::fs::write(dir.join("task.toml"), task).map_err(|e| io(&dir, &e))?;
    }
    Ok(())
}

/// Seeds the base harness (the built-in tree plus the buggy script) once.
fn seed_harness(scope: &ProfileScope) -> Result<(), Error> {
    let dir = HarnessDir::of(scope.dir());
    let active = dir.active_id()?;
    if dir.tree(&active)?.get("scripts/total.py").is_some() {
        return Ok(());
    }
    let mut tree = Tree::builtin();
    tree.files
        .insert("scripts/total.py".to_owned(), BUGGY.to_owned());
    let version = dir.write(&tree, HarnessSource::Import, None)?;
    dir.activate(&version.id)
}

fn read_state(place: &Place) -> State {
    std::fs::read_to_string(place.root().join(STATE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_state(place: &Place, state: &State) -> Result<(), Error> {
    let path = place.root().join(STATE);
    std::fs::write(
        &path,
        serde_json::to_string_pretty(state).unwrap_or_default(),
    )
    .map_err(|e| io(&path, &e))
}

/// Runs the demo; returns its report.
///
/// # Errors
///
/// `demo.output_not_empty` (2), `demo.io`, profile, store and evolution
/// errors.
pub fn run(settings: &Settings, launch: &LaunchEnv) -> Result<Report, Error> {
    let place = Place::prepare_resumable(settings.output.as_deref(), STATE)?;
    let outcome = run_in(&place, settings, launch);
    place.finish(settings.output.is_some());
    outcome
}

fn run_in(place: &Place, settings: &Settings, launch: &LaunchEnv) -> Result<Report, Error> {
    write_scenario(place)?;
    let home = place.home();
    let set = overrides(place);
    let ctx = Context {
        home_flag: Some(&home),
        launch,
        profile: None,
        set: &set,
        json: false,
    };
    let scope = ctx.scope()?;
    seed_harness(&scope)?;
    let store = Arc::new(Store::open(&scope)?);
    let calls_before = calls(&store);
    let mut state = read_state(place);
    let mut report = Report {
        tasks_completed: 0,
        tasks_failed: 0,
        executions: 0,
        editor_calls: 0,
        model_calls: 0,
        tasks: Vec::new(),
    };
    for (id, ledger, expected) in TASKS {
        let task = run_task(
            place,
            settings,
            (&scope, launch, &store),
            (id, ledger),
            &mut state,
        )?;
        write_state(place, &state)?;
        let jobs = Jobs::of(scope.dir());
        let summary = JobSummary::read(&jobs.dir(&task.job)).unwrap_or_default();
        let deliverables = selected(&jobs.dir(&task.job), &summary);
        let completed =
            !deliverables.is_empty() && deliverables.iter().all(|d| d.contains(expected));
        if task.ran {
            let (executions, editors) = fresh_work(&jobs.dir(&task.job));
            report.executions += executions;
            report.editor_calls += editors;
        }
        report.tasks.push(TaskReport {
            task: id.to_owned(),
            job: task.job,
            deliverables,
            expected: expected.to_owned(),
            completed,
            resumed: !task.ran,
        });
    }
    report.tasks_completed = report.tasks.iter().filter(|t| t.completed).count();
    report.tasks_failed = report.tasks.len() - report.tasks_completed;
    report.model_calls = calls(&store).saturating_sub(calls_before);
    let path = place.root().join("demo.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )
    .map_err(|e| io(&path, &e))?;
    Ok(report)
}

fn calls(store: &Store) -> u64 {
    store
        .usage_totals(UsageScope::Profile)
        .map(|t| t.calls)
        .unwrap_or_default()
}

/// A task's job and whether this run ran it.
struct TaskRun {
    job: String,
    ran: bool,
}

/// Queues the task's job the first time, runs it unless it finished, and
/// adopts what it selected.
fn run_task(
    place: &Place,
    settings: &Settings,
    (scope, launch, store): (&ProfileScope, &LaunchEnv, &Arc<Store>),
    (id, ledger): (&str, &str),
    state: &mut State,
) -> Result<TaskRun, Error> {
    let jobs = Jobs::of(scope.dir());
    let known = state
        .tasks
        .get(id)
        .map(|t| t.job.clone())
        .filter(|j| jobs.get(j).is_ok());
    let job = if let Some(job) = known {
        job
    } else {
        let job = queue(place, settings, scope, id, ledger)?;
        state.tasks.insert(
            id.to_owned(),
            TaskState {
                job: job.clone(),
                adopted: Vec::new(),
            },
        );
        job
    };
    let status = jobs.get(&job)?.status;
    // Only a job still to finish runs (an interrupted one was left
    // `running` by the process that stopped).
    let ran = matches!(status.as_str(), "queued" | "running");
    if ran {
        let _ = jobs.requeue_interrupted();
        crate::model_cmd::block_on(engine_run(scope, launch, store, &job))??;
    }
    adopt(
        scope,
        launch,
        &job,
        state.tasks.entry(id.to_owned()).or_default(),
    )?;
    Ok(TaskRun { job, ran })
}

fn queue(
    place: &Place,
    settings: &Settings,
    scope: &ProfileScope,
    id: &str,
    ledger: &str,
) -> Result<String, Error> {
    let store = Store::open(scope)?;
    let profile = crate::evolve_cmd::profile(scope, Some(&store));
    let mut request = Request::new(
        Trigger::Cli,
        TaskInput::File(place.root().join("tasks").join(id).join("task.toml")),
    );
    request.k = Some(settings.k);
    request.mode = Some(settings.mode);
    request.domain = Some("accounting".to_owned());
    let ledger = ledger.to_owned();
    let record = pmt_evolve::job::queue_with(&profile, &request, move |dir| {
        let path = dir.join("source").join("ledger.txt");
        std::fs::write(&path, &ledger).map_err(|e| io(&path, &e))
    })?;
    Ok(record.id)
}

async fn engine_run(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    store: &Arc<Store>,
    job: &str,
) -> Result<(), Error> {
    let rollouts = DemoRollouts {
        inner: HostRollouts {
            scope: scope.clone(),
            launch: launch.clone(),
            store: Arc::clone(store),
        },
    };
    let improver = HostImprover {
        scope: scope.clone(),
        launch: launch.clone(),
        store: Arc::clone(store),
    };
    let rules = crate::harness_host::rules(scope, launch.user_home().map(PathBuf::as_path));
    let config = &scope.config().evolve;
    let engine = Engine {
        profile_dir: scope.dir(),
        config,
        rules: &rules,
        rollouts: &rollouts,
        improver: &improver,
        limits: Limits::process(config),
        retry_delay: Duration::from_millis(10),
        experience: &scope.config().experience,
        estimate_usd: 0.0,
    };
    Box::pin(engine.run(job)).await.map(|_| ())
}

/// Adopts the job's pending proposals (the first of a `both` job; its
/// sibling is superseded).
fn adopt(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    job: &str,
    task: &mut TaskState,
) -> Result<(), Error> {
    let proposals = pmt_evolve::proposal::Proposals::of(scope.dir());
    let rules = crate::harness_host::rules(scope, launch.user_home().map(PathBuf::as_path));
    let settings = crate::experience_cmd::settings(scope);
    for proposal in proposals.list()?.into_iter().filter(|p| p.job_id == job) {
        let current = proposals.get(&proposal.id)?;
        if current.status != pmt_evolve::proposal::Status::Pending {
            continue;
        }
        pmt_evolve::experience::adopt(scope.dir(), &proposal.id, true, &rules, &settings)?;
        task.adopted.push(proposal.id.clone());
    }
    Ok(())
}

/// The final message of each mode's selected candidate.
fn selected(job_dir: &Path, summary: &JobSummary) -> Vec<String> {
    summary
        .modes
        .iter()
        .filter_map(|m| {
            let label = &m.selected.as_ref()?.label;
            let path = job_dir
                .join("modes")
                .join(&m.mode)
                .join(label)
                .join("rollout.json");
            let outcome: RolloutOutcome =
                serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
            outcome.final_message
        })
        .collect()
}

/// Rollouts and proposer sessions the job's ledger records as executed
/// (not served from the cache).
fn fresh_work(job_dir: &Path) -> (usize, usize) {
    let Some(ledger) = std::fs::read_to_string(job_dir.join("ledger.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Ledger>(&t).ok())
    else {
        return (0, 0);
    };
    let entries = ledger
        .modes
        .iter()
        .flat_map(|m| &m.entries)
        .filter(|e| !e.cached);
    let (mut rollouts, mut sessions) = (0, 0);
    for entry in entries {
        match entry.kind {
            Kind::Rollout => rollouts += 1,
            Kind::Session => sessions += 1,
            _ => {}
        }
    }
    (rollouts, sessions)
}

/// `demo --evolve`: prints `demo: <done> tasks completed; <failed> failed`.
///
/// # Errors
///
/// As [`run`].
pub fn demo(settings: &Settings, launch: &LaunchEnv) -> Result<u8, Error> {
    let report = run(settings, launch)?;
    for task in &report.tasks {
        let how = if task.resumed { "resumed" } else { "ran" };
        println!(
            "{}: {how}; selected: {} (expected {})",
            task.task,
            task.deliverables.join(" | "),
            task.expected
        );
    }
    println!(
        "{} executions, {} editor calls, {} model calls",
        report.executions, report.editor_calls, report.model_calls
    );
    println!(
        "demo: {} tasks completed; {} failed",
        report.tasks_completed, report.tasks_failed
    );
    Ok(if report.tasks_failed > 0 {
        pmt_core::exit::FAILURE
    } else {
        pmt_core::exit::SUCCESS
    })
}

/// `evolve demo`: reports each task resumed or ran and
/// `<n> model calls, <n> rollouts`.
///
/// # Errors
///
/// As [`run`].
pub fn evolve_demo(settings: &Settings, launch: &LaunchEnv) -> Result<u8, Error> {
    let report = run(settings, launch)?;
    for task in &report.tasks {
        let how = if task.resumed { "resumed" } else { "ran" };
        println!(
            "{}: {how}; selected: {}",
            task.task,
            task.deliverables.join(" | ")
        );
    }
    println!(
        "{} model calls, {} rollouts",
        report.model_calls, report.executions
    );
    Ok(if report.tasks_failed > 0 {
        pmt_core::exit::FAILURE
    } else {
        pmt_core::exit::SUCCESS
    })
}
