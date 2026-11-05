//! `prometheus approvals suggest` (spec: tool-system, Approval suggestions
//! mined from history): proposals from the approval decisions in the
//! profile's security audit log. Without `--apply` nothing is written;
//! `--apply` merges the chosen patterns into `[approvals].allow` through
//! the same writer as an `always` decision.

use std::fmt::Write as _;
use std::path::PathBuf;

use pmt_config::grants::GrantsFile;
use pmt_core::audit::{AuditLog, Kind};
use pmt_core::error::{Error, codes};
use pmt_tools::approval::Floors;
use pmt_tools::approval::suggest::{Decided, Limits, Proposal, propose};
use serde::Serialize;

use crate::cli::ApprovalsSuggestArgs;
use crate::context::Context;
use crate::output::{document, print_raw};

/// The `approvals suggest` document.
#[derive(Debug, Serialize)]
struct Report {
    /// The window looked at, in days.
    days: u32,
    /// Numbered proposals (`number` is what `--apply` takes).
    proposals: Vec<Numbered>,
    /// Patterns `--apply` added (empty without it).
    applied: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Numbered {
    number: usize,
    #[serde(flatten)]
    proposal: Proposal,
}

/// Runs `approvals suggest`.
///
/// # Errors
///
/// `approvals.no_such_proposal` (exit 2); the audit log or `grants.toml`
/// cannot be read or written.
pub(super) fn run(args: &ApprovalsSuggestArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let grants = GrantsFile::of(scope.profile());
    let allowed = grants.load()?.allow;
    let log = AuditLog::of(scope.dir());
    let entries = log
        .entries()
        .map_err(|e| Error::io(&codes::HOME_IO, log.path(), &e))?;
    let since = jiff::Timestamp::now()
        - jiff::SignedDuration::from_hours(i64::from(args.days).saturating_mul(24));
    let history: Vec<Decided> = entries
        .iter()
        .filter(|e| e.kind == Kind::Approval)
        .filter(|e| e.ts.parse::<jiff::Timestamp>().is_ok_and(|ts| ts >= since))
        .filter_map(|e| Decided::from_record(&e.subject, &e.decision, e.detail.as_deref()))
        .collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let floors = Floors::new(&cwd)
        .user_home(ctx.launch.user_home().map(PathBuf::as_path))
        .prometheus_home(scope.home());
    let proposals = propose(
        &history,
        Limits {
            min_count: args.min_count.max(1),
            limit: args.limit,
            allowed: &allowed,
            floors: &floors,
        },
    );
    let chosen = chosen(&args.apply, &proposals)?;
    let mut applied = Vec::new();
    for pattern in chosen {
        grants.allow_command(&pattern)?;
        applied.push(pattern);
    }
    let report = Report {
        days: args.days,
        proposals: proposals
            .into_iter()
            .enumerate()
            .map(|(i, proposal)| Numbered {
                number: i + 1,
                proposal,
            })
            .collect(),
        applied,
    };
    if ctx.json {
        return document("approvals suggest", &report);
    }
    print_raw(&render(&report))
}

/// The patterns of the 1-based proposal `numbers`, checked before
/// anything is written.
fn chosen(numbers: &[usize], proposals: &[Proposal]) -> Result<Vec<String>, Error> {
    numbers
        .iter()
        .map(|&n| {
            n.checked_sub(1)
                .and_then(|i| proposals.get(i))
                .map(|p| p.pattern.clone())
                .ok_or_else(|| {
                    Error::new(
                        &codes::APPROVALS_NO_SUCH_PROPOSAL,
                        format!(
                            "no proposal {n}: there are {} (run without --apply to list them)",
                            proposals.len()
                        ),
                    )
                })
        })
        .collect()
}

fn render(report: &Report) -> String {
    if report.proposals.is_empty() {
        return format!(
            "no proposals: no command was approved often enough in the last {} days\n",
            report.days
        );
    }
    let mut out = String::new();
    for numbered in &report.proposals {
        let p = &numbered.proposal;
        let _ = writeln!(
            out,
            "{}. {}  ({} approvals; e.g. {})",
            numbered.number,
            p.pattern,
            p.count,
            p.examples.join(", ")
        );
    }
    if report.applied.is_empty() {
        out.push_str("nothing changed; add proposals with --apply <n,...>\n");
    } else {
        for pattern in &report.applied {
            let _ = writeln!(out, "added to [approvals].allow: {pattern}");
        }
    }
    out
}
