//! Proposals (spec: harness-evolution, Proposals and adoption policy;
//! Stale proposal protection; Human-only evaluation of proposals):
//! `proposals`, `review [--eval <suite>]`, `adopt [--now] [--rebase]` and
//! `reject`. Adoption writes a new harness version with `source =
//! "evolve"` that applies from the next session (or, with `--now`, at the
//! next turn of open sessions).

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::Args;
use pmt_config::ProfileScope;
use pmt_config::harness::HarnessDir;
use pmt_core::error::Error;
use pmt_evolve::engine::events::JobLog;
use pmt_evolve::harness::diff;
use pmt_evolve::job::Jobs;
use pmt_evolve::proposal::Proposals;
use serde_json::json;

use crate::context::Context;
use crate::output::{json, print_raw};

/// `evolve review` arguments.
#[derive(Debug, Clone, Args)]
pub struct ReviewArgs {
    /// The proposal.
    pub proposal: String,
    /// Also run this eval suite against the base and the proposed harness
    /// and show you the results (they never reach evolution).
    #[arg(long, value_name = "SUITE")]
    pub eval: Option<String>,
}

/// `evolve proposals`.
///
/// # Errors
///
/// `harness.io`.
pub fn list(ctx: &Context<'_>, scope: &ProfileScope) -> Result<u8, Error> {
    let proposals = Proposals::of(scope.dir()).list()?;
    if ctx.json {
        return json(&proposals);
    }
    let mut text = String::new();
    for p in &proposals {
        let _ = writeln!(
            text,
            "{}  {:<10} {} {:<10} {}  base {}  {}",
            p.id,
            p.status.as_str(),
            p.job_id,
            p.mode,
            p.label,
            p.base_version,
            p.diff_summary
        );
    }
    print_raw(&text)
}

/// `evolve review <proposal> [--eval <suite>]`.
///
/// # Errors
///
/// `evolve.proposal_not_found`; `harness.*`; an unknown eval suite.
pub fn review(ctx: &Context<'_>, scope: &ProfileScope, args: &ReviewArgs) -> Result<u8, Error> {
    let store = Proposals::of(scope.dir());
    let proposal = store.get(&args.proposal)?;
    let tree = store.tree(&proposal.id)?;
    let base = HarnessDir::of(scope.dir()).tree(&proposal.base_version)?;
    let unified = diff::unified(&base, &tree, (&proposal.base_version, &proposal.id));
    let eval = match &args.eval {
        Some(suite) => {
            let sides = super::review_eval::Sides {
                proposal: &proposal.id,
                base_version: &proposal.base_version,
                proposed: &tree,
            };
            Some(super::review_eval::run(scope, ctx.launch, &sides, suite)?)
        }
        None => None,
    };
    if ctx.json {
        return json(&json!({
            "proposal": proposal,
            "diff": unified,
            "eval": eval.as_ref().map(super::review_eval::Report::json),
        }));
    }
    let census = &proposal.census;
    let mut text = format!(
        "proposal {} ({}, job {}, {} final {}), base {}\n{}\n\n",
        proposal.id,
        proposal.status.as_str(),
        proposal.job_id,
        proposal.mode,
        proposal.label,
        proposal.base_version,
        proposal.diff_summary
    );
    text.push_str(&unified);
    let _ = writeln!(
        text,
        "\ncensus: {} files, {} always-on chars, {} skills, {} agents, {} scripts, {} hooks",
        census.files,
        census.always_on_chars,
        census.skills,
        census.agents,
        census.scripts,
        census.hooks
    );
    let _ = writeln!(text, "selector reason: {}", or_none(&proposal.reason));
    text.push_str(&lesson_lines(&proposal));
    if proposal.approval_required {
        text.push_str("approval required: it adds or changes scripts, hooks or subagents\n");
    }
    if let Some(report) = &eval {
        text.push_str(&report.render());
    }
    print_raw(&text)
}

/// The lesson part of `review`: the lesson, or what adopting will (not)
/// add to the experience library.
fn lesson_lines(proposal: &pmt_evolve::proposal::Proposal) -> String {
    let Some(lesson) = &proposal.lesson else {
        return "lesson: none\nno transferable lesson; nothing will be added to the experience library\n".to_owned();
    };
    let mut text = format!(
        "lesson ({}): {}\n  cue: {}\n  lesson: {}\n  evidence: {}\n  confidence: {}\n",
        proposal.domain,
        lesson.lesson,
        lesson.cue,
        lesson.lesson,
        lesson.evidence,
        lesson.confidence_line()
    );
    if let Some(why) = &proposal.lesson_rejected {
        let _ = writeln!(
            text,
            "lesson rejected ({why}): no experience entry was written"
        );
    }
    text
}

fn or_none(text: &str) -> &str {
    if text.trim().is_empty() {
        "(none)"
    } else {
        text
    }
}

/// `evolve adopt <proposal> [--now] [--rebase]`.
///
/// # Errors
///
/// `evolve.proposal_not_found`; `evolve.proposal_refused` (65) when it is
/// stale, not pending or invalid.
pub fn adopt(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    id: &str,
    now: bool,
    rebase: bool,
) -> Result<u8, Error> {
    let rules = crate::harness_host::rules(scope, ctx.launch.user_home().map(PathBuf::as_path));
    let settings = crate::experience_cmd::settings(scope);
    let (adopted, library) =
        pmt_evolve::experience::adopt(scope.dir(), id, rebase, &rules, &settings)?;
    crate::harness_cmd::record_activation(scope, &adopted.from, &adopted.version);
    let job_dir = Jobs::of(scope.dir()).dir(&adopted.proposal.job_id);
    if job_dir.is_dir() {
        JobLog::new(&job_dir, &adopted.proposal.job_id)
            .emit("evolve.proposal.adopted", json!({"proposal_id": id}));
    }
    let marked = if now {
        let store = pmt_store::Store::open(scope)?;
        Some(crate::harness_host::now::activate(
            scope,
            &store,
            &adopted.version.id,
        )?)
    } else {
        None
    };
    if ctx.json {
        return json(&json!({
            "proposal_id": id,
            "from": adopted.from,
            "to": adopted.version.id,
            "superseded": adopted.superseded,
            "rebuilt_sessions": marked,
            "experience": library.message(),
        }));
    }
    let when = match marked {
        Some(n) => format!("{n} open session(s) rebuild their prompt at their next turn"),
        None => "takes effect from the next session".to_owned(),
    };
    let mut text = format!(
        "adopted {id} as {} (from {}); {when}\n",
        adopted.version.id, adopted.from
    );
    for other in &adopted.superseded {
        let _ = writeln!(text, "superseded {other}");
    }
    let _ = writeln!(text, "{}", library.message());
    print_raw(&text)
}

/// `evolve reject <proposal>`.
///
/// # Errors
///
/// `evolve.proposal_not_found`; `evolve.proposal_refused`.
pub fn reject(ctx: &Context<'_>, scope: &ProfileScope, id: &str) -> Result<u8, Error> {
    let proposal = Proposals::of(scope.dir()).reject(id)?;
    if ctx.json {
        return json(&proposal);
    }
    print_raw(&format!("rejected {id}\n"))
}
