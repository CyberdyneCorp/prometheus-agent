//! `prometheus evolve` and `/evolve` (spec: harness-evolution, Opt-in per
//! profile; On-demand and scheduled triggers; Modes, budget and labels).
//!
//! Every trigger goes through [`pmt_evolve::job::queue`]: the opt-in gate
//! first (exit 78, nothing written), then the budget (exit 2), then the
//! frozen task (exit 65). A foreground `evolve run` then runs the job in
//! this process ([`jobs`]); `--detach` leaves it to the daemon's runner.
//! Proposals are reviewed, adopted and rejected through [`proposals`].

pub mod auto;
pub mod cron;
pub mod jobs;
pub mod kanban;
pub mod proposals;
mod review_eval;
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
    /// Run an evolution job for a recorded session or a task file (in the
    /// foreground, unless --detach).
    Run(RunArgs),
    /// The profile's evolution jobs, oldest first.
    List,
    /// A job's status, stages, selection and spend by kind.
    Status {
        /// The job.
        job: String,
    },
    /// Stop a job at its next model-call boundary.
    Cancel {
        /// The job.
        job: String,
    },
    /// Run an interrupted job again from its caches (in the foreground).
    Resume {
        /// The job.
        job: String,
    },
    /// The anonymised candidates of a job awaiting your selection.
    Compare {
        /// The job.
        job: String,
    },
    /// Pick a candidate of a job awaiting your selection and continue it.
    Choose {
        /// The job.
        job: String,
        /// The candidate's id (`A`, `B`...).
        id: String,
    },
    /// The profile's harness proposals, oldest first.
    Proposals,
    /// The offline evolution demo: two accounting tasks through the whole
    /// pipeline on the mock provider, in a profile of its own (needs
    /// neither `evolve.enabled` nor credentials).
    Demo(DemoArgs),
    /// A proposal's diff against its base version, census, selector
    /// reason and lesson.
    Review(proposals::ReviewArgs),
    /// Adopt a proposal as a new harness version (from the next session).
    Adopt {
        /// The proposal.
        proposal: String,
        /// Also rebuild the prompt of open sessions at their next turn.
        #[arg(long)]
        now: bool,
        /// Apply the proposal onto a changed active harness when its diff
        /// applies cleanly.
        #[arg(long)]
        rebase: bool,
    },
    /// Reject a pending proposal.
    Reject {
        /// The proposal.
        proposal: String,
    },
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
    /// Return once the job is queued (the daemon runs it).
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
    if let EvolveCommand::Demo(args) = command {
        let settings = crate::demo_cmd::evolve::Settings {
            k: args.k,
            mode: args.mode.mode(),
            output: args.output.clone(),
        };
        return crate::demo_cmd::evolve::evolve_demo(&settings, ctx.launch);
    }
    let scope = ctx.scope()?;
    pmt_evolve::gate::require_enabled(scope.config(), scope.name().as_str())?;
    match command {
        EvolveCommand::Run(args) => run_job(ctx, &scope, args),
        EvolveCommand::List => list(ctx, &scope),
        EvolveCommand::Status { job } => jobs::status(ctx, &scope, job),
        EvolveCommand::Cancel { job } => jobs::cancel(ctx, &scope, job),
        EvolveCommand::Resume { job } => jobs::resume(ctx, &scope, job),
        EvolveCommand::Compare { job } => jobs::compare(ctx, &scope, job),
        EvolveCommand::Choose { job, id } => jobs::choose(ctx, &scope, job, id),
        EvolveCommand::Proposals => proposals::list(ctx, &scope),
        EvolveCommand::Review(args) => proposals::review(ctx, &scope, args),
        EvolveCommand::Adopt {
            proposal,
            now,
            rebase,
        } => proposals::adopt(ctx, &scope, proposal, *now, *rebase),
        EvolveCommand::Reject { proposal } => proposals::reject(ctx, &scope, proposal),
        EvolveCommand::Demo(_) => unreachable!("run() handles the demo first"),
    }
}

/// `prometheus evolve demo` flags.
#[derive(Debug, Clone, Args)]
pub struct DemoArgs {
    /// Candidates per mode (1 or 2 show the whole pipeline).
    #[arg(long, value_name = "N", default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=8))]
    pub k: u8,
    /// The evolution modes.
    #[arg(long, value_enum, default_value = "parallel")]
    pub mode: crate::cli::DemoMode,
    /// Run in DIR (created when missing; an earlier demo there resumes).
    #[arg(long, value_name = "DIR")]
    pub output: Option<PathBuf>,
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
    if args.detach {
        return report(ctx, &record);
    }
    if !ctx.json {
        print_raw(&format!("{}\n", queued_line(&record)))?;
    }
    jobs::foreground(ctx, scope, &record.id)
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
