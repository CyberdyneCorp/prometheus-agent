//! `prometheus trajectory compress` (spec: replay-and-evaluation,
//! Trajectory compressor): `ShareGPT` records over the token target keep
//! their head and tail and get one summary turn in place of the smallest
//! middle span that brings them under it; summaries come from the
//! `trajectory_compress` auxiliary route (`--model` wins), recorded with
//! purpose `trajectory.compress` and accounted to a hidden session.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Args;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_eval::compress::{self, Outcome, Settings, Stats, Summarizer, Turn};
use pmt_eval::grader::BoxFuture;
use pmt_providers::auxiliary::{AuxArgs, AuxTask};
use pmt_store::{NewSession, Source, Store, UsageScope};
use serde_json::{Value, json};

use crate::chat::session::model::{Models, RouteFlags};
use crate::context::Context;

/// `prometheus trajectory compress`.
#[derive(Debug, Clone, Args)]
pub struct CompressArgs {
    /// The `ShareGPT` JSON-lines file.
    pub input: PathBuf,
    /// The output file (default: `<input stem>.compressed.jsonl`).
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub output: Option<PathBuf>,
    /// Records over this many tokens are compressed.
    #[arg(long, value_name = "N", default_value_t = compress::TARGET_MAX_TOKENS)]
    pub target_max_tokens: u64,
    /// The summary's token budget.
    #[arg(long, value_name = "N", default_value_t = compress::SUMMARY_TARGET_TOKENS)]
    pub summary_target_tokens: u64,
    /// Turns kept verbatim at the end.
    #[arg(long, value_name = "N", default_value_t = compress::PROTECT_LAST_TURNS)]
    pub protect_last_turns: usize,
    /// Process a deterministic sample of this percentage of the records.
    #[arg(long, value_name = "P", default_value_t = 100.0)]
    pub sample_percent: f64,
    /// The summary model (`provider/model` or a model).
    #[arg(long, value_name = "M")]
    pub model: Option<String>,
    /// Summaries made at once.
    #[arg(long, value_name = "N", default_value_t = 4)]
    pub parallel: usize,
}

const SYSTEM: &str = "You summarise a span of an agent's conversation so that it can replace the span in a training record. Keep what later turns rely on: the facts found, files and commands involved, decisions and open problems. Answer with the summary only.";

struct RouteSummarizer {
    models: Models,
    session: SessionId,
    args: AuxArgs,
}

impl Summarizer for RouteSummarizer {
    fn summarize<'a>(
        &'a self,
        turns: &'a [Turn],
        max_tokens: u64,
    ) -> BoxFuture<'a, Result<(String, Option<f64>), String>> {
        Box::pin(async move {
            let span = turns.iter().fold(String::new(), |mut out, t| {
                let _ = write!(out, "[{}]\n{}\n\n", t.from, t.value);
                out
            });
            let user = format!("Summarise in at most {max_tokens} tokens:\n\n{span}");
            match self
                .models
                .ask_auxiliary_with(
                    AuxTask::TrajectoryCompress,
                    &self.args,
                    self.session,
                    (SYSTEM, &user),
                    max_tokens,
                )
                .await
            {
                Ok(Some(text)) => Ok((text, None)),
                Ok(None) => Err("no summary model route".to_owned()),
                Err(err) => Err(err.message().to_owned()),
            }
        })
    }
}

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::io(&codes::EVAL_IO, path, e)
}

/// `<dir>/<stem><suffix>` of `path`.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{stem}{suffix}"))
}

fn read_records(path: &Path) -> Result<Vec<Value>, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| io(path, &e))?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str(l).map_err(|e| {
                Error::new(
                    &codes::EVAL_INVALID_DATASET,
                    format!("{} line {}: {e}", path.display(), i + 1),
                )
            })
        })
        .collect()
}

fn write_lines(path: &Path, records: &[Value]) -> Result<(), Error> {
    let text = records.iter().fold(String::new(), |mut out, r| {
        let _ = writeln!(out, "{r}");
        out
    });
    std::fs::write(path, text).map_err(|e| io(path, &e))
}

/// Runs `prometheus trajectory compress`.
///
/// # Errors
///
/// `eval.invalid_dataset` (65) for a line that is not JSON; `eval.io`;
/// profile and store errors.
pub fn run(args: &CompressArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let records = read_records(&args.input)?;
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| beside(&args.input, ".compressed.jsonl"));
    let store = Arc::new(Store::open(&scope)?);
    // Untitled on purpose: titles are unique and capped at 100 chars, so one
    // derived from the input path fails on long paths and on a second run
    // over the same file. The hidden session is found by its source tag.
    let session = store
        .create_session(&NewSession {
            source_tag: Some("trajectory-compress".to_owned()),
            hidden: true,
            ..NewSession::new(Source::Eval)
        })?
        .id;
    let summarizer = RouteSummarizer {
        models: Models::open(
            &scope,
            &store,
            ctx.launch.user_home().map(PathBuf::as_path),
            &RouteFlags::default(),
            None,
        )?,
        session,
        args: crate::eval_cmd::judge::aux_args(args.model.as_deref()),
    };
    let mut stats = Stats {
        records: records.len(),
        ..Stats::default()
    };
    let sample: Vec<Value> = records
        .into_iter()
        .enumerate()
        .filter(|(i, _)| compress::sampled(*i, args.sample_percent))
        .map(|(_, r)| r)
        .collect();
    stats.sampled = sample.len();
    let settings = Settings {
        target_max_tokens: args.target_max_tokens,
        summary_target_tokens: args.summary_target_tokens,
        protect_last_turns: args.protect_last_turns,
    };
    let done = crate::model_cmd::block_on(compress::compress_all(
        sample,
        settings,
        &summarizer,
        args.parallel,
    ))?;
    let (mut kept, mut over) = (Vec::new(), Vec::new());
    for d in &done {
        stats.add(d);
        match &d.outcome {
            Outcome::Unchanged(r) | Outcome::Compressed(r) => kept.push(r.clone()),
            Outcome::OverLimit(r) => over.push(r.clone()),
            Outcome::Failed(why) => {
                tracing::warn!(reason = %why, "trajectory record not compressed");
            }
        }
    }
    if let Ok(totals) = store.usage_totals(UsageScope::Session(session)) {
        stats.cost_usd = totals.cost_usd;
        stats.unpriced_calls = usize::try_from(totals.unpriced_calls).unwrap_or(usize::MAX);
    }
    write_lines(&output, &kept)?;
    let over_path = beside(&output, ".over_limit.jsonl");
    if !over.is_empty() {
        write_lines(&over_path, &over)?;
    }
    let stats_path = output.with_file_name("compress-stats.json");
    pmt_eval::run::write_json(&stats_path, &stats)?;
    if ctx.json {
        return crate::output::document(
            "trajectory compress",
            &json!({"output": output, "stats": stats_path, "over_limit": over.len(), "summary": stats}),
        );
    }
    crate::output::print(&format!(
        "{} records: {} unchanged, {} compressed, {} over the limit{}, {} failed; tokens {} -> {} ({})",
        stats.sampled,
        stats.unchanged,
        stats.compressed,
        stats.over_limit,
        if over.is_empty() {
            String::new()
        } else {
            format!(" ({})", over_path.display())
        },
        stats.failed,
        stats.tokens_before,
        stats.tokens_after,
        output.display()
    ))
}
