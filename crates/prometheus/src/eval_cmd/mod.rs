//! `prometheus eval` (spec: replay-and-evaluation, Eval suite format; Eval
//! run; Eval runs are resumable; Answer-free boundary; Post-hoc judging;
//! Re-judging and judge identity; Eval reports and comparison; Benchmark
//! adapter protocol; Session-to-task capture; Batch prompt datasets;
//! Batch statistics and quality filtering; Toolset distributions).
//!
//! The parts that need no session live in `pmt-eval`; this module runs
//! task sessions ([`exec`]), orchestrates runs ([`run`], [`batch`]) and
//! wires the judge's script and model sides ([`judge`]).

pub mod batch;
pub mod capture;
pub mod exec;
pub mod judge;
pub mod report;
pub mod run;
pub mod suites;
pub mod trace;

use clap::{Args, Subcommand, ValueEnum};
use pmt_core::error::Error;

use crate::context::Context;

/// `prometheus eval` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum EvalCommand {
    /// Check a suite's format; exit 65 naming the first invalid file.
    Validate {
        /// The suite: a directory, or a name under `<profile>/evals/`.
        suite: String,
    },
    /// The suites under `<profile>/evals/` and the bundled ones.
    List,
    /// Run a suite (or `bench:<name>[:<split>]`) in isolated sessions.
    Run(RunArgs),
    /// Score a run's executed tasks (the only reader of grader material).
    Judge(JudgeArgs),
    /// Scores, costs and comparisons of judged runs.
    Report(ReportArgs),
    /// Turn a recorded session into a regression task of a suite.
    Capture(capture::CaptureArgs),
    /// Run a JSON-lines prompt dataset as ungraded tasks.
    Batch(batch::BatchArgs),
    /// The built-in toolset distributions and their probabilities.
    Distributions,
}

/// `--cache` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CacheFlag {
    /// Neither read nor written.
    Off,
    /// Stored responses are served; nothing new is stored.
    Read,
    /// Both.
    #[value(name = "read_write", alias = "read-write")]
    ReadWrite,
}

impl CacheFlag {
    /// The `replay.cache.mode` value.
    #[must_use]
    pub const fn mode(self) -> pmt_config::schema::CacheMode {
        match self {
            Self::Off => pmt_config::schema::CacheMode::Off,
            Self::Read => pmt_config::schema::CacheMode::Read,
            Self::ReadWrite => pmt_config::schema::CacheMode::ReadWrite,
        }
    }

    /// Its name in `run.json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Read => "read",
            Self::ReadWrite => "read_write",
        }
    }
}

/// What `eval run` and `eval batch` share.
#[derive(Debug, Clone, Args)]
pub struct CommonArgs {
    /// The provider of the task sessions.
    #[arg(long, value_name = "P")]
    pub provider: Option<String>,
    /// The model.
    #[arg(long, short = 'm', value_name = "M")]
    pub model: Option<String>,
    /// Concurrent tasks.
    #[arg(long, value_name = "N", default_value_t = 4)]
    pub parallel: usize,
    /// The model-call cache.
    #[arg(long, value_enum, default_value = "read_write")]
    pub cache: CacheFlag,
    /// Stop the run once its spend would exceed this (exit 3).
    #[arg(long, value_name = "X")]
    pub budget_usd: Option<f64>,
    /// Resume (or name) this run.
    #[arg(long, value_name = "ID")]
    pub run_id: Option<String>,
    /// Sample each task's toolsets from a distribution (a built-in name
    /// or a TOML file).
    #[arg(long, value_name = "NAME|FILE")]
    pub toolset_distribution: Option<String>,
    /// The sampling seed.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub seed: u64,
}

/// `prometheus eval run`.
#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    /// The suite: a directory, a name under `<profile>/evals/`, a bundled
    /// suite (`smoke`) or `bench:<name>[:<split>]`.
    pub suite: String,
    /// The harness version (`h3` or an id; default: the active one).
    #[arg(long, value_name = "REF")]
    pub harness: Option<String>,
    /// Only these tasks.
    #[arg(long, value_name = "a,b", value_delimiter = ',')]
    pub tasks: Vec<String>,
    /// Runs per task.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub repeat: u32,
    /// Shared flags.
    #[command(flatten)]
    pub common: CommonArgs,
}

/// `prometheus eval judge`.
#[derive(Debug, Clone, Args)]
pub struct JudgeArgs {
    /// The run.
    pub run_id: String,
    /// Tasks scored at once.
    #[arg(long, value_name = "N", default_value_t = 8)]
    pub parallel: usize,
    /// Score into `judged.<NAME>.json` (another judge or grader version).
    #[arg(long, value_name = "NAME")]
    pub judge_namespace: Option<String>,
}

/// `eval report --format` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReportFormat {
    /// Plain text.
    Text,
    /// Markdown tables.
    Markdown,
    /// One JSON document.
    Json,
}

/// `prometheus eval report`.
#[derive(Debug, Clone, Args)]
pub struct ReportArgs {
    /// The runs; the first is the comparison base.
    #[arg(required = true)]
    pub run_ids: Vec<String>,
    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    pub format: ReportFormat,
    /// Read `judged.<NAME>.json`.
    #[arg(long, value_name = "NAME")]
    pub judge_namespace: Option<String>,
}

/// The run ids `eval` makes: `run_<ulid>` (lower case: a run id is also
/// its sessions' source tag).
#[must_use]
pub fn new_run_id() -> String {
    format!("run_{}", ulid::Ulid::generate().to_string().to_lowercase())
}

/// `--run-id`, or a new one; a run id must be a valid source tag.
///
/// # Errors
///
/// `session.invalid_source_tag` (2).
pub fn run_id(given: Option<&str>) -> Result<String, Error> {
    let id = given.map_or_else(new_run_id, str::to_owned);
    pmt_store::sessions::check_source_tag(&id)?;
    Ok(id)
}

/// Runs `prometheus eval`.
///
/// # Errors
///
/// As each subcommand.
pub fn run(command: &EvalCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        EvalCommand::Validate { suite } => suites::validate(suite, ctx),
        EvalCommand::List => suites::list(ctx),
        EvalCommand::Run(args) => run::run(args, ctx),
        EvalCommand::Judge(args) => judge::run(args, ctx),
        EvalCommand::Report(args) => report::run(args, ctx),
        EvalCommand::Capture(args) => capture::run(args, ctx),
        EvalCommand::Batch(args) => batch::run(args, ctx),
        EvalCommand::Distributions => batch::distributions(ctx),
    }
}
