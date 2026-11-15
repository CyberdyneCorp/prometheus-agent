//! `prometheus evolve` and `/evolve` (spec: harness-evolution, Opt-in per
//! profile; On-demand and scheduled triggers; Modes, budget and labels).
//!
//! Every trigger goes through [`pmt_evolve::job::queue`]: the opt-in gate
//! first (exit 78, nothing written), then the budget (exit 2), then the
//! frozen task (exit 65). This build queues jobs; running them is the
//! evolution engine's.

pub mod auto;
pub mod cron;
pub mod kanban;
pub mod slash;

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use pmt_config::ProfileScope;
use pmt_config::schema::EvolveMode;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_evolve::job::{self, JobRecord, Profile, Request, TaskInput, Trigger};
use serde_json::json;

use crate::context::Context;
use crate::output::{json, print_raw};

/// `prometheus evolve` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum EvolveCommand {
    /// Queue an evolution job for a recorded session or a task file.
    Run(RunArgs),
    /// The profile's evolution jobs, oldest first.
    List,
}

/// `prometheus evolve run` arguments.
#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    /// The session whose task to evolve for.
    #[arg(
        long,
        value_name = "ID",
        required_unless_present = "task",
        conflicts_with = "task"
    )]
    pub session: Option<String>,
    /// A task file (TOML: `statement`, optional `domain`, `workspace`,
    /// `toolsets`, `model`).
    #[arg(long, value_name = "FILE")]
    pub task: Option<PathBuf>,
    /// Recursion budget k (1-8): 4k rollouts and 3k improver sessions per
    /// mode (default `evolve.k`).
    #[arg(long, value_name = "N")]
    pub k: Option<u8>,
    /// Search mode (default `evolve.mode`).
    #[arg(long, value_enum)]
    pub mode: Option<ModeArg>,
    /// Experience-library domain (default: the task file's, else
    /// `general`).
    #[arg(long, value_name = "D")]
    pub domain: Option<String>,
    /// Stop the job when its spend reaches this many US dollars.
    #[arg(long, value_name = "X")]
    pub budget_usd: Option<f64>,
    /// Return once the job is queued.
    #[arg(long)]
    pub detach: bool,
    /// Edit the task statement (`task.md`) in $EDITOR before the job is
    /// queued.
    #[arg(long)]
    pub edit: bool,
}

/// `--mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ModeArg {
    /// Width: independent compositions.
    Parallel,
    /// Depth: rounds of checkpoints.
    Sequential,
    /// Both, on shared base rollouts.
    Both,
}

impl From<ModeArg> for EvolveMode {
    fn from(mode: ModeArg) -> Self {
        match mode {
            ModeArg::Parallel => Self::Parallel,
            ModeArg::Sequential => Self::Sequential,
            ModeArg::Both => Self::Both,
        }
    }
}

/// Runs one `evolve` subcommand.
///
/// # Errors
///
/// `evolve.disabled` (78), `cli.usage` (2), `evolve.invalid_task` (65).
pub fn run(command: &EvolveCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    pmt_evolve::gate::require_enabled(scope.config(), scope.name().as_str())?;
    match command {
        EvolveCommand::Run(args) => run_job(ctx, &scope, args),
        EvolveCommand::List => list(ctx, &scope),
    }
}

/// The profile handle jobs are queued in.
pub(crate) fn profile<'a>(
    scope: &'a ProfileScope,
    store: Option<&'a pmt_store::Store>,
) -> Profile<'a> {
    Profile {
        dir: scope.dir(),
        name: scope.name().as_str(),
        config: scope.config(),
        store,
        redactor: scope.secrets().redactor().unwrap_or_default(),
    }
}

fn run_job(ctx: &Context<'_>, scope: &ProfileScope, args: &RunArgs) -> Result<u8, Error> {
    let k = job::budget::check_k(args.k.unwrap_or(scope.config().evolve.k))?;
    let (task, store) = match (&args.session, &args.task) {
        (Some(id), _) => (
            TaskInput::Session(SessionId::parse(id)?),
            Some(pmt_store::Store::open(scope)?),
        ),
        (None, Some(file)) => (TaskInput::File(file.clone()), None),
        (None, None) => unreachable!("clap requires --session or --task"),
    };
    let request = Request {
        k: Some(k),
        mode: args.mode.map(Into::into),
        domain: args.domain.clone(),
        budget_usd: args.budget_usd,
        ..Request::new(Trigger::Cli, task)
    };
    let profile = profile(scope, store.as_ref());
    let record = if args.edit {
        let editor = ctx
            .launch
            .editor()
            .unwrap_or(crate::config_cmd::edit::DEFAULT_EDITOR);
        job::queue_with(&profile, &request, |task| {
            crate::config_cmd::edit::launch_editor(editor, &task.join("task.md"))
        })?
    } else {
        job::queue(&profile, &request)?
    };
    report(ctx, &record)
}

fn report(ctx: &Context<'_>, record: &JobRecord) -> Result<u8, Error> {
    if ctx.json {
        return json(&record_json(record));
    }
    print_raw(&format!("{}\n", queued_line(record)))
}

/// `job evo_… queued (k=2, parallel: 8 rollouts, 6 improver sessions)`.
#[must_use]
pub fn queued_line(record: &JobRecord) -> String {
    let plans: Vec<String> = record
        .plan
        .iter()
        .map(|p| {
            format!(
                "{}: {} rollouts, {} improver sessions",
                p.mode, p.rollouts, p.sessions
            )
        })
        .collect();
    format!(
        "job {} queued (k={}, {})",
        record.id,
        record.k,
        plans.join("; ")
    )
}

fn record_json(record: &JobRecord) -> serde_json::Value {
    serde_json::to_value(record).unwrap_or_else(|_| json!({"id": record.id}))
}

fn list(ctx: &Context<'_>, scope: &ProfileScope) -> Result<u8, Error> {
    let jobs = job::Jobs::of(scope.dir()).list()?;
    if ctx.json {
        return json(&jobs.iter().map(record_json).collect::<Vec<_>>());
    }
    let mut text = String::new();
    for j in &jobs {
        let _ = writeln!(
            text,
            "{}  {:<8} {:<7} k={} {:<10} {}  {}",
            j.id,
            j.status,
            j.trigger.as_str(),
            j.k,
            j.mode,
            j.domain,
            j.created_at
        );
    }
    print_raw(&text)
}
